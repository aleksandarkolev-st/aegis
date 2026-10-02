//! Text interaction for the same commands exposed by the terminal.
//! Execution is journaled locally before dispatch; a crash never replays an edit.
use std::path::Path;

use anyhow::{Context, Result, bail};
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};

use super::{Authority, Receipt, require_actor_run_connection};
use crate::storage::Store;

fn schema(authority: &Authority) -> Result<()> {
    authority.store.connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS remote_actor_options (
            actor_id TEXT PRIMARY KEY REFERENCES remote_actors(actor_id), value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_command_reviews (
            token TEXT PRIMARY KEY, actor_id TEXT NOT NULL REFERENCES remote_actors(actor_id),
            task_id TEXT NOT NULL REFERENCES runs(id), command TEXT NOT NULL,
            snapshot TEXT NOT NULL, expires_at INTEGER NOT NULL
         );
         CREATE TABLE IF NOT EXISTS remote_reply_pages (
            actor_id TEXT PRIMARY KEY REFERENCES remote_actors(actor_id), text TEXT NOT NULL,
            task_id TEXT REFERENCES runs(id)
         );",
    )?;
    Ok(())
}

pub(super) fn preferences(authority: &Authority, actor: &str) -> Result<Value> {
    schema(authority)?;
    let value: Option<String> = authority
        .store
        .connection
        .query_row(
            "SELECT value FROM remote_actor_options WHERE actor_id=?1",
            [actor],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .unwrap_or_else(|| Ok(json!({})))
}

fn save_preferences(authority: &Authority, actor: &str, value: &Value) -> Result<()> {
    authority.store.connection.execute(
        "INSERT INTO remote_actor_options(actor_id,value) VALUES (?1,?2)
         ON CONFLICT(actor_id) DO UPDATE SET value=excluded.value",
        params![actor, value.to_string()],
    )?;
    Ok(())
}

pub(super) fn goal_submission(text: &str) -> Option<&str> {
    let (command, body) = text.trim().split_once(char::is_whitespace)?;
    if !matches!(command, "/goal" | "/contract") {
        return None;
    }
    let body = body.trim();
    if body.is_empty()
        || matches!(
            body.split_whitespace().next()?,
            "add" | "replace" | "history"
        )
    {
        None
    } else {
        Some(body)
    }
}

pub(super) fn execute(
    root: &Path,
    workspace: &Path,
    authority: &mut Authority,
    actor: &str,
    text: &str,
    mut receipt: Receipt,
) -> Result<Receipt> {
    if receipt.result["slash_pending"] != true {
        return Ok(receipt);
    }
    schema(authority)?;
    let mut interrupted = receipt.result.clone();
    interrupted
        .as_object_mut()
        .context("invalid slash receipt")?
        .remove("slash_pending");
    interrupted["reply"] = json!(
        "This command began but its final reply was not recorded. Inspect the saved state before submitting an edit again. Aegis will not replay it automatically."
    );
    let changed = authority.store.connection.execute(
        "UPDATE remote_requests SET result=?3 WHERE actor_id=?1 AND request_id=?2
         AND json_extract(result,'$.slash_pending')=1",
        params![actor, receipt.request_id, interrupted.to_string()],
    )?;
    if changed == 0 {
        let saved: String = authority.store.connection.query_row(
            "SELECT result FROM remote_requests WHERE actor_id=?1 AND request_id=?2",
            params![actor, receipt.request_id],
            |row| row.get(0),
        )?;
        receipt.result = serde_json::from_str(&saved)?;
        return Ok(receipt);
    }
    let task_id = receipt.result["task_id"].as_str().map(str::to_owned);
    let response = match dispatch(
        root,
        workspace,
        authority,
        actor,
        task_id.as_deref(),
        text.trim(),
    ) {
        Ok(reply) => reply,
        Err(error) => format!("Command could not be applied: {error}"),
    };
    let response = super::redact_remote_text(&crate::text::clean(&response));
    // Pages contain only locally authorized command output. Re-check the saved
    // task's grant every time a subsequent page is requested.
    let reply = if text.starts_with("/more") {
        response
    } else {
        authority.store.connection.execute(
            "INSERT INTO remote_reply_pages(actor_id,text,task_id) VALUES (?1,?2,?3)
             ON CONFLICT(actor_id) DO UPDATE SET text=excluded.text,task_id=excluded.task_id",
            params![actor, response, task_id],
        )?;
        page(&response, 1)?
    };
    receipt.result = json!({"reply":reply, "task_id":task_id});
    authority.store.connection.execute(
        "UPDATE remote_requests SET result=?3 WHERE actor_id=?1 AND request_id=?2",
        params![actor, receipt.request_id, receipt.result.to_string()],
    )?;
    Ok(receipt)
}

fn page(text: &str, number: usize) -> Result<String> {
    if number == 0 {
        bail!("Pages start at 1");
    }
    let mut pages = Vec::new();
    let mut start = 0;
    while start < text.len() {
        let mut end = (start + 3400).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end < text.len()
            && let Some(newline) = text[start..end].rfind('\n')
            && newline > 2400
        {
            end = start + newline + 1;
        }
        pages.push(&text[start..end]);
        start = end;
    }
    if pages.is_empty() {
        pages.push("No saved entries.");
    }
    let selected = pages.get(number - 1).context("That page does not exist")?;
    if pages.len() > 1 {
        Ok(format!(
            "{selected}\n\nPage {number}/{}. /more {}",
            pages.len(),
            number + 1
        ))
    } else {
        Ok((*selected).to_owned())
    }
}

fn selected<'a>(task: Option<&'a str>) -> Result<&'a str> {
    task.context("No task selected. Use /sessions or send /goal followed by task text")
}

fn display(value: &impl serde::Serialize) -> Result<String> {
    Ok(serde_json::to_string_pretty(value)?)
}

fn dispatch(
    root: &Path,
    workspace: &Path,
    authority: &mut Authority,
    actor: &str,
    task: Option<&str>,
    text: &str,
) -> Result<String> {
    let (command, rest) = text
        .split_once(char::is_whitespace)
        .map_or((text, ""), |(a, b)| (a, b.trim()));
    let name = command.trim_start_matches('/');
    match name {
        "" | "help" => {
            let filter = rest.trim_start_matches('/');
            let mut lines = vec!["Aegis commands (send /help <prefix> to filter):".to_owned()];
            for (name, description) in crate::commands::COMMANDS {
                if filter.is_empty() || name.trim_start_matches('/').starts_with(filter) {
                    lines.push(format!("{name} — {description}"));
                }
            }
            lines.push("Phone: /use <task>, /result, /details, /approve_once <code>, /deny <code>, /confirm <code>, /back, /more <page>.\n/tasks lists shared tasks; /tasks plan shows selected milestones.\nPickers return commands with arguments. /exit detaches this conversation.".into());
            Ok(lines.join("\n"))
        }
        "more" => {
            let (text, scope): (String, Option<String>) = authority
                .store
                .connection
                .query_row(
                    "SELECT text,task_id FROM remote_reply_pages WHERE actor_id=?1",
                    [actor],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?
                .context("No command reply to paginate")?;
            if let Some(scope) = scope {
                require_actor_run_connection(&authority.store.connection, actor, &scope)?;
            }
            page(&text, rest.parse().context("Use /more <page number>")?)
        }
        "back" => {
            authority.store.connection.execute(
                "DELETE FROM remote_command_reviews WHERE actor_id=?1",
                [actor],
            )?;
            Ok("Pending command reviews discarded. Your task is unchanged.".into())
        }
        "confirm" => confirm(root, authority, actor, rest),
        "goal" | "contract"
            if matches!(rest.split_whitespace().next(), Some("add" | "replace")) =>
        {
            review(authority, actor, selected(task)?, text)
        }
        "goal" | "contract" | "why" | "verify" | "provider" | "budget" | "handoff" | "evidence" => {
            let store = &authority.store;
            let value = crate::control::view(
                store,
                selected(task)?,
                name,
                if rest.is_empty() { None } else { Some(rest) },
            )?;
            let output = crate::control::display(name, &value);
            Ok(if output.trim().is_empty() {
                "No saved entries for this view.".into()
            } else {
                output
            })
        }
        "sessions" | "chats" => {
            if rest.is_empty() {
                return Ok(format!(
                    "{}\nSelect with /sessions <alias> or /use <alias>.",
                    display(&authority.authorized_runs(actor)?)?
                ));
            }
            let run = resolve_reference(authority, actor, rest)?;
            authority.bind_selected_task(actor, &run)?;
            Ok(format!(
                "Selected task {rest}. Send /status, /goal, /resume, or a message."
            ))
        }
        "providers" | "model" | "models" | "reasoning" | "settings" => {
            profile_command(root, authority, actor, name, rest)
        }
        "login" => login(root, authority, actor, rest),
        "memory" => memory(workspace, &mut authority.store, rest),
        "instructions" => instructions(workspace, &mut authority.store, rest),
        "context" => {
            let run = authority.store.run(selected(task)?)?;
            let value = json!({"task":run.task,"state":run.state,"provider":authority.store.current_route(&run.id)?,
                "requirements":authority.store.obligations(&run.id)?,"checkpoint":authority.store.last_checkpoint(&run.id)?,
                "instructions":crate::instructions::frozen(&run.budgets)?,"repository_rules":crate::repository_rules::frozen(&run.budgets)?,
                "usage":crate::control::view(&authority.store,&run.id,"budget",None)?});
            display(&value)
        }
        "tools" => {
            let run = authority.store.run(selected(task)?)?;
            display(
                &json!({"grants":run.grants,"active":authority.store.working_capabilities(&run.id)?}),
            )
        }
        "artifacts" => artifacts(&authority.store, selected(task)?, rest),
        "trace" => {
            let events = authority.store.events(selected(task)?)?;
            Ok(events
                .iter()
                .map(|event| format!("{} {} {}", event.seq, event.created_at, event.kind))
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "tasks" if rest == "plan" => display(&authority.store.milestones(selected(task)?)?),
        "checkpoint" => display(&authority.store.last_checkpoint(selected(task)?)?),
        _ => Ok(format!(
            "Unknown command: {command}. Send /help to see all commands. This was not sent to the model."
        )),
    }
}

fn resolve_reference(authority: &Authority, actor: &str, reference: &str) -> Result<String> {
    let transaction = authority.store.connection.unchecked_transaction()?;
    let run = super::resolve_actor_task_reference(&transaction, actor, reference)?;
    transaction.commit()?;
    Ok(run)
}

fn profile_command(
    root: &Path,
    authority: &Authority,
    actor: &str,
    command: &str,
    rest: &str,
) -> Result<String> {
    let mut prefs = preferences(authority, actor)?;
    let profile = crate::session::remote_profile(root, &prefs)?;
    match command {
        "providers" => {
            if rest.is_empty() {
                return Ok(format!(
                    "Current: {}. Select /providers chatgpt or /providers grok. The provider configured locally ({}) is also available. Selection applies to new tasks.",
                    profile["provider"], profile["provider"]
                ));
            }
            prefs["provider"] = json!(crate::provider::canonical(rest));
            prefs.as_object_mut().unwrap().remove("model");
            prefs.as_object_mut().unwrap().remove("reasoning_effort");
            crate::session::remote_profile(root, &prefs)?;
            // Resolve a real account default immediately; never reuse another
            // provider's model ID in a new task.
            let catalog = crate::session::remote_catalog(root, &prefs, false)?;
            prefs["model"] = json!(
                catalog
                    .models
                    .first()
                    .context("Provider returned no models")?
                    .id
            );
        }
        "model" | "models" => {
            let catalog = crate::session::remote_catalog(root, &prefs, rest == "refresh")?;
            if rest.is_empty() || rest == "refresh" {
                return Ok(format!(
                    "{}\n{}\nChoose /model <id>. Applies to new tasks.",
                    catalog.source,
                    catalog
                        .models
                        .iter()
                        .map(|m| format!("{} — {}", m.id, m.label))
                        .collect::<Vec<_>>()
                        .join("\n")
                ));
            }
            let chosen = catalog
                .models
                .iter()
                .find(|model| model.id == rest)
                .context("Choose an ID listed by /models")?;
            prefs["model"] = json!(chosen.id);
            prefs.as_object_mut().unwrap().remove("reasoning_effort");
        }
        "reasoning" => {
            let provider = profile["provider"]
                .as_str()
                .context("No provider configured")?;
            let model = profile["model"].as_str().context("Choose /model first")?;
            let levels = crate::catalog::reasoning_levels(provider, model)?;
            if rest.is_empty() {
                return Ok(format!(
                    "Current: {}. Choose /reasoning <effort>: {}. Applies to new tasks.",
                    profile["reasoning_effort"],
                    levels.join(", ")
                ));
            }
            if !levels.iter().any(|level| level == rest) {
                bail!("Choose a supported effort: {}", levels.join(", "));
            }
            prefs["reasoning_effort"] = json!(rest);
        }
        "settings" => {
            if rest.is_empty() {
                return Ok(format!(
                    "{}\nSet /settings time <seconds>, /settings command-time <seconds>, /settings model-time <seconds>, /settings context <characters>, /settings write on|off. Applies to new tasks. Tool permissions come from your local profile. Phone replies are plain text.",
                    display(&profile)?
                ));
            }
            let (field, value) = rest
                .split_once(char::is_whitespace)
                .context("Use /settings <field> <value>")?;
            if field == "write" {
                prefs["write"] = match value.trim() {
                    "on" => json!(true),
                    "off" => json!(false),
                    _ => bail!("Use on or off"),
                };
            } else {
                let key = match field {
                    "time" => "wall_seconds",
                    "command-time" => "process_seconds",
                    "model-time" => "model_seconds",
                    "context" => "context_chars",
                    _ => bail!("Unknown setting; send /settings"),
                };
                prefs["limits"] = profile["limits"].clone();
                prefs["limits"][key] = json!(
                    value
                        .trim()
                        .parse::<u64>()
                        .context("Use a positive integer")?
                );
            }
        }
        _ => unreachable!(),
    }
    crate::session::remote_profile(root, &prefs)?;
    save_preferences(authority, actor, &prefs)?;
    Ok(format!(
        "Saved for new tasks: {}",
        display(&crate::session::remote_profile(root, &prefs)?)?
    ))
}

fn login(root: &Path, authority: &Authority, actor: &str, rest: &str) -> Result<String> {
    let profile = crate::session::remote_profile(root, &preferences(authority, actor)?)?;
    let name = profile["provider"]
        .as_str()
        .context("No provider selected")?;
    let provider = crate::direct::provider(name)?;
    let vault = crate::auth_store::Vault::user()?;
    let key = match provider {
        crate::direct::Provider::ChatGpt => "chatgpt",
        crate::direct::Provider::Grok => "grok",
    };
    if rest == "status" {
        return Ok(if vault.load(key)?.is_some() {
            "Aegis has a saved account sign-in."
        } else {
            "No saved sign-in. Send /login to obtain a device code."
        }
        .into());
    }
    if !rest.is_empty() {
        bail!("Use /login or /login status. Credentials are saved on your PC");
    }
    let client = crate::oauth::AuthClient::new(provider)?;
    let mut pending = client.begin(|| false)?;
    let reply = format!(
        "Open {} and enter {}. Code expires in {} seconds. Aegis is waiting on your PC; use /login status after authorizing.",
        pending.verification_url(),
        pending.user_code(),
        pending.expires_in().as_secs()
    );
    std::thread::spawn(move || {
        loop {
            match client.poll(&mut pending, || false) {
                Ok(crate::oauth::Poll::Pending(delay)) => {
                    std::thread::sleep(delay.min(std::time::Duration::from_secs(10)))
                }
                Ok(crate::oauth::Poll::SignedIn(session)) => {
                    let _ = client.save(&vault, &session, || false);
                    break;
                }
                Err(_) => break,
            }
        }
    });
    Ok(reply)
}

fn memory(workspace: &Path, store: &mut Store, rest: &str) -> Result<String> {
    let (action, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match action {
        "" => Ok(format!(
            "{}\n/memory add <text>, /memory replace <id> <text>, /memory remove <id>",
            display(&store.project_memory(workspace)?)?
        )),
        "add" => Ok(format!(
            "Saved note {}.",
            store.remember(workspace, body, None)?
        )),
        "replace" => {
            let (id, text) = body
                .split_once(char::is_whitespace)
                .context("Use /memory replace <id> <text>")?;
            Ok(format!(
                "Updated note {}.",
                store.remember(workspace, text, Some(id))?
            ))
        }
        "remove" => {
            store.forget(workspace, body)?;
            Ok("Note removed.".into())
        }
        _ => bail!("Use /memory to see notes and edit commands"),
    }
}

fn instructions(workspace: &Path, store: &mut Store, rest: &str) -> Result<String> {
    let (action, body) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
    match action {
        "" => Ok(format!(
            "{}\n/instructions add <scope> <text>, /instructions replace <id> <scope> <text>, /instructions remove <id>. Changes apply to new tasks.",
            display(&store.project_instructions(workspace)?)?
        )),
        "add" | "replace" => {
            let (replace, body) = if action == "replace" {
                let (id, body) = body
                    .split_once(char::is_whitespace)
                    .context("Use /instructions replace <id> <scope> <text>")?;
                (Some(id), body)
            } else {
                (None, body)
            };
            let (scope, text) = body
                .split_once(char::is_whitespace)
                .context("Supply scope and instruction text")?;
            Ok(format!(
                "Saved instruction {} for new tasks.",
                store.pin_instruction(workspace, scope, text, replace)?
            ))
        }
        "remove" => {
            store.unpin_instruction(workspace, body)?;
            Ok("Instruction removed for new tasks.".into())
        }
        _ => bail!("Use /instructions to see rules and edit commands"),
    }
}

fn artifacts(store: &Store, task: &str, rest: &str) -> Result<String> {
    let available = store.inspectable_artifacts(task)?;
    if rest.is_empty() {
        return Ok(format!(
            "{}\n/artifacts <hash> [query] inspects a saved artifact. Query examples: @keys, @slice 0 2000, @lines 1 40.",
            display(&available)?
        ));
    }
    let (hash, query) = rest
        .split_once(char::is_whitespace)
        .unwrap_or((rest, "@slice 0 2000"));
    if !available.iter().any(|(id, _)| id == hash) {
        bail!("Artifact is not inspectable in this selected task");
    }
    Ok(crate::kernel::inspect(&store.artifact(hash)?, query))
}

fn snapshot(store: &Store, task: &str) -> Result<String> {
    Ok(serde_json::to_string(
        &json!({"requirements":store.obligations(task)?,"revision":store.workspace_revision(task)?}),
    )?)
}

fn review(authority: &Authority, actor: &str, task: &str, text: &str) -> Result<String> {
    let run = authority.store.run(task)?;
    if run.is_terminal() || matches!(run.state.as_str(), "ready" | "running") {
        bail!("Pause this task and wait for the safe boundary before changing requirements");
    }
    let (_, rest) = text
        .split_once(char::is_whitespace)
        .context("Use /goal add <title> | <reason> or /goal replace O3 <title> | <reason>")?;
    let (_, body) = rest
        .trim()
        .split_once(char::is_whitespace)
        .context("Supply requirement text and a reason separated by |")?;
    let (title, reason) = body
        .split_once('|')
        .context("Supply requirement text | reason")?;
    if title.trim().is_empty() || reason.trim().is_empty() {
        bail!("Requirement and reason cannot be empty");
    }
    let token = uuid::Uuid::new_v4().to_string();
    authority.store.connection.execute(
        "INSERT INTO remote_command_reviews(token,actor_id,task_id,command,snapshot,expires_at) VALUES (?1,?2,?3,?4,?5,?6)",
        params![token,actor,task,text,snapshot(&authority.store,task)?,crate::storage::unix_time()+300],
    )?;
    Ok(format!(
        "Review for task {task}:\n{text}\nSend /confirm {token} within 5 minutes to apply this exact change, or /back to discard."
    ))
}

fn confirm(root: &Path, authority: &mut Authority, actor: &str, token: &str) -> Result<String> {
    let (task,text,expected,expires): (String,String,String,i64) = authority.store.connection.query_row(
        "SELECT task_id,command,snapshot,expires_at FROM remote_command_reviews WHERE token=?1 AND actor_id=?2",params![token,actor],
        |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    ).optional()?.context("Review not found or already consumed")?;
    require_actor_run_connection(&authority.store.connection, actor, &task)?;
    if expires <= crate::storage::unix_time() || expected != snapshot(&authority.store, &task)? {
        bail!("Review expired or requirements changed; review the change again");
    }
    let run = authority.store.run(&task)?;
    if run.is_terminal()
        || matches!(run.state.as_str(), "ready" | "running")
        || crate::kernel::is_active(root, &task)?
    {
        bail!("Task must remain paused before confirming");
    }
    authority.store.connection.execute(
        "DELETE FROM remote_command_reviews WHERE token=?1 AND actor_id=?2",
        params![token, actor],
    )?;
    let (_, rest) = text.split_once(char::is_whitespace).unwrap();
    let (action, body) = rest.trim().split_once(char::is_whitespace).unwrap();
    let (title, reason) = body.split_once('|').unwrap();
    let reason = reason.trim();
    let id = match action {
        "add" => authority
            .store
            .add_obligation(&task, title.trim(), reason)?,
        "replace" => {
            let (id, title) = title
                .trim()
                .split_once(char::is_whitespace)
                .context("Supply /goal replace O3 <title> | <reason>")?;
            authority.store.supersede_obligation(
                &task,
                crate::control::obligation_id(id)?,
                title.trim(),
                reason,
            )?
        }
        _ => bail!("Unsupported review action"),
    };
    Ok(format!(
        "Requirement O{id} is open. Send /goal to inspect it and /resume to continue."
    ))
}

#[cfg(test)]
mod tests {
    use super::super::{Command, CommandEnvelope};
    use super::*;

    #[test]
    fn goal_parser_preserves_multiline_and_never_consumes_contract_edits() {
        assert_eq!(
            goal_submission("/goal Do work\nRequirements:\n- test it"),
            Some("Do work\nRequirements:\n- test it")
        );
        for value in [
            "/goal",
            "/goal history",
            "/goal add test",
            "/contract replace O3 test",
            "/goalkeeper hi",
        ] {
            assert!(goal_submission(value).is_none());
        }
    }

    #[test]
    fn pages_keep_all_unicode_without_exceeding_transport_limit() -> Result<()> {
        let text = "Привіт 🌍\n".repeat(1500);
        let mut rebuilt = String::new();
        for number in 1..100 {
            let Ok(value) = page(&text, number) else {
                break;
            };
            assert!(value.len() < 4096);
            rebuilt.push_str(value.split("\n\nPage ").next().unwrap());
        }
        assert_eq!(rebuilt, text);
        Ok(())
    }

    #[test]
    fn authenticated_commands_discover_catalog_and_edit_once_without_inference() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join(".arun");
        let mut authority = Authority::open(&root)?;
        authority.register_actor("phone")?;
        authority.register_actor("other")?;
        let request = |text: &str| CommandEnvelope {
            version: 1,
            installation_id: authority.installation_id().into(),
            actor_id: "phone".into(),
            request_id: uuid::Uuid::new_v4().to_string(),
            issued_at: crate::storage::unix_time(),
            expires_at: crate::storage::unix_time() + 300,
            command: Command::Slash { text: text.into() },
        };
        let help = request("/");
        let add = request("/memory add keep small commits");
        let now = crate::storage::unix_time();
        assert!(
            authority
                .apply_from_authenticated_relay("other", &add, now)
                .is_err()
        );
        let receipt = authority.apply_from_authenticated_relay("phone", &help, now)?;
        let reply = execute(&root, temp.path(), &mut authority, "phone", "/", receipt)?;
        for (command, _) in crate::commands::COMMANDS {
            assert!(
                reply.result["reply"].as_str().unwrap().contains(command),
                "missing {command}"
            );
        }
        let receipt = authority.apply_from_authenticated_relay("phone", &add, now)?;
        execute(
            &root,
            temp.path(),
            &mut authority,
            "phone",
            "/memory add keep small commits",
            receipt,
        )?;
        let duplicate = authority.apply_from_authenticated_relay("phone", &add, now)?;
        execute(
            &root,
            temp.path(),
            &mut authority,
            "phone",
            "/memory add keep small commits",
            duplicate,
        )?;
        assert_eq!(authority.store.project_memory(temp.path())?.len(), 1);
        assert!(authority.store.runs()?.is_empty());
        Ok(())
    }

    #[test]
    fn phone_views_reviews_and_preferences_preserve_scope_and_frozen_grants() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let root = temp.path().join(".arun");
        let mut authority = Authority::open(&root)?;
        authority.register_actor("phone")?;
        authority.register_actor("other")?;
        std::fs::write(
            root.join("profile.json"),
            serde_json::to_vec(
                &json!({"provider":"codex","model":"gpt-6.1-sol","reasoning_effort":"high","endpoint":null,"write":false,"image":null}),
            )?,
        )?;
        let run = authority.store.create_run(
            "Repair API\nRequirements:\n- preserve behavior",
            temp.path(),
            "codex",
            json!(["workspace.read"]),
            json!({"model":"gpt-6.1-sol"}),
            "verify work",
        )?;
        authority.store.state(&run.id, "paused", json!({}))?;
        authority.grant_run("phone", &run.id)?;
        authority.bind_selected_task("phone", &run.id)?;
        schema(&authority)?;
        for command in [
            "/goal",
            "/contract history",
            "/why",
            "/verify",
            "/provider",
            "/provider history",
            "/budget",
            "/handoff",
            "/context",
            "/tools",
            "/artifacts",
            "/trace",
            "/tasks plan",
            "/checkpoint",
            "/sessions",
            "/settings",
            "/memory",
            "/instructions",
        ] {
            assert!(
                !dispatch(
                    &root,
                    temp.path(),
                    &mut authority,
                    "phone",
                    Some(&run.id),
                    command
                )?
                .is_empty(),
                "{command}"
            );
        }
        assert!(profile_command(&root, &authority, "phone", "settings", "write on").is_err());
        assert!(profile_command(&root, &authority, "phone", "settings", "time 0").is_err());
        assert_eq!(preferences(&authority, "phone")?, json!({}));
        let proposal = review(
            &authority,
            "phone",
            &run.id,
            "/goal add Cover failure case | user requested it",
        )?;
        let token = proposal
            .split("/confirm ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        assert!(confirm(&root, &mut authority, "other", token).is_err());
        confirm(&root, &mut authority, "phone", token)?;
        assert!(confirm(&root, &mut authority, "phone", token).is_err());
        assert_eq!(
            authority
                .store
                .obligations(&run.id)?
                .iter()
                .filter(|o| o.title == "Cover failure case")
                .count(),
            1
        );
        assert_eq!(
            authority.store.run(&run.id)?.grants,
            json!(["workspace.read"])
        );
        let stale = review(
            &authority,
            "phone",
            &run.id,
            "/goal replace O1 Changed requirement | reviewed",
        )?;
        let token = stale
            .split("/confirm ")
            .nth(1)
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap();
        authority
            .store
            .add_obligation(&run.id, "Peer change", "reviewed locally")?;
        assert!(confirm(&root, &mut authority, "phone", token).is_err());
        authority.store.connection.execute("INSERT INTO remote_reply_pages(actor_id,text,task_id) VALUES ('phone','private content',?1)",[&run.id])?;
        authority.revoke_run("phone", &run.id)?;
        assert!(dispatch(&root, temp.path(), &mut authority, "phone", None, "/more 1").is_err());
        Ok(())
    }
}
