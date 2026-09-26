use std::path::Path;

use anyhow::{Result, bail};
use rusqlite::{Transaction, params};
use serde::{Deserialize, Serialize};

use crate::storage::{Run, Store};

struct Rule {
    category: &'static str,
    choice: &'static str,
    preference: &'static str,
    phrases: &'static [&'static str],
}

const RULES: &[Rule] = &[
    Rule {
        category: "package_manager",
        choice: "pnpm",
        preference: "Prefer pnpm for Node package management; inspect the project's lockfile first.",
        phrases: &["use pnpm", "prefer pnpm"],
    },
    Rule {
        category: "package_manager",
        choice: "npm",
        preference: "Prefer npm for Node package management; inspect the project's lockfile first.",
        phrases: &["use npm", "prefer npm"],
    },
    Rule {
        category: "package_manager",
        choice: "yarn",
        preference: "Prefer Yarn for Node package management; inspect the project's lockfile first.",
        phrases: &["use yarn", "prefer yarn"],
    },
    Rule {
        category: "package_manager",
        choice: "bun",
        preference: "Prefer Bun for Node package management; inspect the project's lockfile first.",
        phrases: &["use bun", "prefer bun"],
    },
    Rule {
        category: "change_size",
        choice: "focused",
        preference: "Prefer small, focused changes; avoid unrelated edits.",
        phrases: &[
            "small focused changes",
            "small changes",
            "keep changes small",
            "minimal changes",
        ],
    },
    Rule {
        category: "change_size",
        choice: "expansive",
        preference: "Allow broader changes when the current task calls for them.",
        phrases: &["prefer broader changes", "prefer broad changes"],
    },
    Rule {
        category: "commits",
        choice: "atomic",
        preference: "Prefer small atomic commits when the current task authorizes committing.",
        phrases: &["small atomic commits", "atomic commits"],
    },
    Rule {
        category: "commits",
        choice: "single",
        preference: "Prefer a single commit when the current task authorizes committing.",
        phrases: &["prefer a single commit", "use a single commit"],
    },
    Rule {
        category: "verification",
        choice: "each_change",
        preference: "Prefer focused functional checks after each change; obey current testing constraints.",
        phrases: &[
            "run tests after each change",
            "test after each change",
            "run tests after every change",
        ],
    },
    Rule {
        category: "verification",
        choice: "final",
        preference: "Defer agent evaluations until implementation and functional verification are complete; obey the current task's verification timing.",
        phrases: &[
            "test it when everything is done",
            "test only when everything is done",
            "benchmarks when everything is done",
            "benchmarks after verification",
        ],
    },
    Rule {
        category: "explanations",
        choice: "concise",
        preference: "Prefer concise progress updates and explanations.",
        phrases: &[
            "keep explanations brief",
            "explain briefly",
            "keep replies short",
            "be concise",
        ],
    },
    Rule {
        category: "explanations",
        choice: "detailed",
        preference: "Prefer detailed explanations of important decisions.",
        phrases: &["explain in detail", "prefer detailed explanations"],
    },
    Rule {
        category: "dependencies",
        choice: "avoid",
        preference: "Avoid adding dependencies unless the current task explicitly requires them.",
        phrases: &[
            "no new dependencies",
            "avoid new dependencies",
            "do not add dependencies",
            "don't add dependencies",
        ],
    },
    Rule {
        category: "dependencies",
        choice: "allowed",
        preference: "Additional dependencies are acceptable if justified by the current task.",
        phrases: &["new dependencies are fine", "allow new dependencies"],
    },
    Rule {
        category: "comments",
        choice: "avoid",
        preference: "Avoid adding inline comments unless explicitly requested.",
        phrases: &[
            "no inline comments",
            "avoid inline comments",
            "do not add inline comments",
            "don't add inline comments",
        ],
    },
    Rule {
        category: "comments",
        choice: "explanatory",
        preference: "Prefer explanatory comments for non-obvious code when appropriate.",
        phrases: &["prefer explanatory comments", "add explanatory comments"],
    },
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Habit {
    pub category: String,
    pub choice: String,
    pub observations: usize,
    pub confirmed: bool,
    pub preference: String,
}

pub fn choices(category: &str) -> Vec<(String, String)> {
    RULES
        .iter()
        .filter(|rule| rule.category == category)
        .map(|rule| (rule.choice.to_owned(), rule.preference.to_owned()))
        .collect()
}

fn matches_phrase(text: &str, phrase: &str) -> bool {
    text.match_indices(phrase).any(|(offset, _)| {
        let before = &text[..offset];
        let after = &text[offset + phrase.len()..];
        if before
            .chars()
            .next_back()
            .is_some_and(|character| character.is_ascii_alphanumeric())
            || after
                .chars()
                .next()
                .is_some_and(|character| character.is_ascii_alphanumeric())
        {
            return false;
        }
        let prefix = before
            .rsplit(['.', ';', ',', ':', '\n'])
            .next()
            .unwrap_or("")
            .rsplit(" and ")
            .next()
            .unwrap_or("")
            .rsplit(" but ")
            .next()
            .unwrap_or("");
        let words: Vec<_> = prefix.split_whitespace().rev().take(8).collect();
        !words
            .iter()
            .any(|word| matches!(*word, "don't" | "never" | "not" | "cannot"))
    })
}

fn observations(task: &str) -> Vec<&'static Rule> {
    let normalized = task
        .chars()
        .take(16_000)
        .collect::<String>()
        .to_ascii_lowercase()
        .replace('’', "'")
        .replace("everything's", "everything is");
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    let matches: Vec<_> = RULES
        .iter()
        .filter(|rule| {
            rule.phrases
                .iter()
                .any(|phrase| matches_phrase(&normalized, phrase))
        })
        .collect();
    matches
        .iter()
        .copied()
        .filter(|rule| {
            matches
                .iter()
                .filter(|candidate| candidate.category == rule.category)
                .count()
                == 1
        })
        .collect()
}

pub(crate) fn record(transaction: &Transaction<'_>, run: &Run) -> Result<()> {
    if run.budgets["learning_enabled"] != true {
        return Ok(());
    }
    for rule in observations(&run.task) {
        transaction.execute("INSERT INTO user_habits(workspace,category,choice,observations,confirmed,last_run,updated_at) VALUES (?1,?2,?3,1,0,?4,?5) ON CONFLICT(workspace,category) DO UPDATE SET observations=CASE WHEN choice=excluded.choice AND updated_at>=excluded.updated_at-2592000 THEN MIN(observations+1,255) ELSE 1 END,confirmed=CASE WHEN choice=excluded.choice THEN confirmed ELSE 0 END,choice=excluded.choice,last_run=excluded.last_run,updated_at=excluded.updated_at", params![run.workspace,rule.category,rule.choice,run.id,crate::storage::unix_time()])?;
    }
    Ok(())
}

impl Store {
    pub fn habits(&self, workspace: &Path) -> Result<Vec<Habit>> {
        let workspace = dunce::canonicalize(workspace)?;
        let mut statement = self.connection.prepare("SELECT category,choice,observations,confirmed FROM user_habits WHERE workspace=?1 AND updated_at>=?2 ORDER BY updated_at DESC,category LIMIT 8")?;
        let rows = statement.query_map(
            params![
                workspace.to_string_lossy(),
                crate::storage::unix_time() - 30 * 86400
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, usize>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            },
        )?;
        let mut habits = Vec::new();
        for row in rows {
            let (category, choice, observations, confirmed) = row?;
            if let Some(rule) = RULES
                .iter()
                .find(|rule| rule.category == category && rule.choice == choice)
            {
                habits.push(Habit {
                    category,
                    choice,
                    observations,
                    confirmed,
                    preference: rule.preference.into(),
                });
            }
        }
        Ok(habits)
    }

    pub fn learned_habits(&self, workspace: &Path, task: &str) -> Result<Vec<Habit>> {
        if !self.learning_enabled(workspace)? {
            return Ok(Vec::new());
        }
        let current = observations(task);
        Ok(self
            .habits(workspace)?
            .into_iter()
            .filter(|habit| {
                (habit.confirmed || habit.observations >= 2)
                    && !current
                        .iter()
                        .any(|rule| rule.category == habit.category && rule.choice != habit.choice)
            })
            .take(4)
            .collect())
    }

    pub fn confirm_habit(&mut self, workspace: &Path, category: &str, choice: &str) -> Result<()> {
        if !RULES
            .iter()
            .any(|rule| rule.category == category && rule.choice == choice)
        {
            bail!("unknown habit choice");
        }
        let workspace = dunce::canonicalize(workspace)?;
        self.connection.execute("INSERT INTO user_habits(workspace,category,choice,observations,confirmed,last_run,updated_at) VALUES (?1,?2,?3,1,1,NULL,?4) ON CONFLICT(workspace,category) DO UPDATE SET choice=excluded.choice,observations=1,confirmed=1,last_run=NULL,updated_at=excluded.updated_at", params![workspace.to_string_lossy(),category,choice,crate::storage::unix_time()])?;
        Ok(())
    }

    pub fn forget_habit(&mut self, workspace: &Path, category: &str) -> Result<()> {
        let workspace = dunce::canonicalize(workspace)?;
        self.connection.execute(
            "DELETE FROM user_habits WHERE workspace=?1 AND category=?2",
            params![workspace.to_string_lossy(), category],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn request(store: &mut Store, workspace: &Path, task: &str) -> Result<Run> {
        store.create_run(task, workspace, "fixture", json!([]), json!({}), "")
    }

    #[test]
    fn repeated_user_preferences_adapt_and_corrections_replace_them_without_model_calls()
    -> Result<()> {
        let directory = tempfile::tempdir()?;
        let other = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        request(
            &mut store,
            directory.path(),
            "Use pnpm and keep changes small",
        )?;
        assert!(
            store
                .learned_habits(directory.path(), "repair parser")?
                .is_empty()
        );
        request(
            &mut store,
            directory.path(),
            "Please use pnpm. Keep changes small.",
        )?;
        let learned = request(&mut store, directory.path(), "repair parser")?;
        assert_eq!(learned.budgets["user_habits"].as_array().unwrap().len(), 2);
        assert_eq!(store.events(&learned.id)?.len(), 1);
        assert!(
            store
                .learned_habits(other.path(), "repair parser")?
                .is_empty()
        );
        assert_eq!(
            store
                .learned_habits(directory.path(), "Use npm instead")?
                .len(),
            1
        );
        request(&mut store, directory.path(), "Use npm instead")?;
        assert_eq!(
            store
                .learned_habits(directory.path(), "repair parser")?
                .len(),
            1
        );
        store.confirm_habit(directory.path(), "package_manager", "npm")?;
        assert_eq!(
            store
                .learned_habits(directory.path(), "repair parser")?
                .len(),
            2
        );
        store.forget_habit(directory.path(), "package_manager")?;
        assert_eq!(
            store
                .learned_habits(directory.path(), "repair parser")?
                .len(),
            1
        );
        store.reset_learning(directory.path())?;
        assert!(store.habits(directory.path())?.is_empty());
        assert_eq!(
            store.run(&learned.id)?.budgets["user_habits"],
            learned.budgets["user_habits"]
        );
        store.set_learning(directory.path(), false)?;
        request(&mut store, directory.path(), "Use pnpm")?;
        assert!(store.habits(directory.path())?.is_empty());
        Ok(())
    }

    #[test]
    fn negations_ambiguity_expiry_and_failed_contracts_do_not_create_habits() -> Result<()> {
        assert!(observations("don't use npm; never prefer pnpm").is_empty());
        assert!(observations("I don't want you to use npm").is_empty());
        assert_eq!(observations("Don't use npm and use pnpm instead").len(), 1);
        assert_eq!(
            observations("Test it when everything's done and verified").len(),
            1
        );
        assert!(observations("use npm or use pnpm").is_empty());
        assert!(observations("use npmx").is_empty());
        assert_eq!(
            observations("Don't add dependencies; no inline comments").len(),
            2
        );
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        assert!(
            store
                .create_run(
                    "Use pnpm",
                    directory.path(),
                    "fixture",
                    json!([]),
                    json!({"acceptance_check":{"invalid":true}}),
                    ""
                )
                .is_err()
        );
        assert!(store.habits(directory.path())?.is_empty());
        store.confirm_habit(directory.path(), "package_manager", "pnpm")?;
        assert!(
            store
                .confirm_habit(
                    directory.path(),
                    "package_manager",
                    "execute arbitrary code"
                )
                .is_err()
        );
        store
            .connection
            .execute("UPDATE user_habits SET updated_at=0", [])?;
        assert!(store.learned_habits(directory.path(), "")?.is_empty());
        assert!(
            store
                .connection
                .query_row("SELECT COUNT(*) FROM user_habits", [], |row| row
                    .get::<_, usize>(0))?
                <= 8
        );
        Ok(())
    }
}
