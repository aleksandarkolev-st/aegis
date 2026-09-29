use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use serde_json::{Value, json};

fn profile() -> Value {
    json!({"provider":"codex", "model":"old-model", "endpoint":null, "write":true, "image":"approved-image", "previous_run":"saved-task", "limits":{"actions":99,"wall_seconds":10800,"model_tokens":123456,"tool_result_tokens":800000,"model_seconds":180,"process_seconds":900,"context_chars":256000,"model_response_bytes":8388608}})
}

#[test]
fn owned_models_and_reasoning_work_without_native_metadata_or_clis() -> Result<()> {
    for provider in [
        arun::direct::Provider::ChatGpt,
        arun::direct::Provider::Grok,
    ] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let mut original = profile();
        original["provider"] = json!(if provider == arun::direct::Provider::ChatGpt {
            "codex"
        } else {
            "grok"
        });
        original["model"] = json!("owned-first");
        original["reasoning_effort"] = json!("low");
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let vault = arun::auth_store::Vault::new(directory.path().join(".aegis/auth"));
        let session = arun::auth_store::Session {
            provider: if provider == arun::direct::Provider::ChatGpt {
                "chatgpt"
            } else {
                "grok"
            }
            .into(),
            access_token: "owned-fixture-private-access".into(),
            refresh_token: None,
            account_id: (provider == arun::direct::Provider::ChatGpt)
                .then(|| "owned-private-account".into()),
            expires_at: 2000000000,
        };
        vault.save(&session)?;
        arun::provider_catalog::save(
            &vault,
            provider,
            &session.credentials()?,
            ["owned-first", "owned-second"]
                .into_iter()
                .map(|id| arun::catalog::RemoteModel {
                    id: id.into(),
                    label: "Same friendly label".into(),
                    reasoning_levels: vec!["low".into(), "high".into()],
                    default_reasoning: Some("low".into()),
                })
                .collect(),
            || false,
        )?;
        let original_session = fs::read(
            directory
                .path()
                .join(format!(".aegis/auth/{}.session", session.provider)),
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .env("CODEX_HOME", directory.path().join("missing-native-cache"))
            .env("PATH", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(b"/model\n2\n/reasoning\n3\n/quit\n")?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(
            text.contains("Same friendly label [owned-first]")
                && text.contains("Same friendly label [owned-second]")
        );
        assert!(text.contains("Aegis account-bound catalog"));
        assert!(text.contains("Provider default · low · no override"));
        assert!(!text.contains("owned-fixture-private-access"));
        assert!(!text.contains("owned-private-account"));
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        assert_eq!(saved["model"], "owned-second");
        assert_eq!(saved["reasoning_effort"], "high");
        for key in ["provider", "limits", "write", "image", "previous_run"] {
            assert_eq!(saved[key], original[key]);
        }
        assert_eq!(
            fs::read(
                directory
                    .path()
                    .join(format!(".aegis/auth/{}.session", session.provider))
            )?,
            original_session
        );
        assert!(!directory.path().join(".grok").exists());
        assert!(!directory.path().join("missing-native-cache").exists());
    }
    Ok(())
}

#[test]
fn expired_owned_accounts_do_not_offer_external_or_old_account_catalogs() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let mut original = profile();
    original["reasoning_effort"] = json!("high");
    fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
    let home = directory.path().join("native-cache");
    fs::create_dir(&home)?;
    fs::write(
        home.join("models_cache.json"),
        serde_json::to_vec(&json!({"models":[{"slug":"external-model","visibility":"list"}]}))?,
    )?;
    let vault = arun::auth_store::Vault::new(directory.path().join(".aegis/auth"));
    let mut session = arun::auth_store::Session {
        provider: "chatgpt".into(),
        access_token: "expired-fixture-access".into(),
        refresh_token: None,
        account_id: Some("expired-private-account".into()),
        expires_at: 2000000000,
    };
    vault.save(&session)?;
    arun::provider_catalog::save(
        &vault,
        arun::direct::Provider::ChatGpt,
        &session.credentials()?,
        vec![arun::catalog::RemoteModel {
            id: "old-account-model".into(),
            label: "Old private catalog".into(),
            reasoning_levels: vec!["low".into()],
            default_reasoning: Some("low".into()),
        }],
        || false,
    )?;
    session.expires_at = 1;
    vault.save(&session)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/model\n1\n/reasoning\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(!text.contains("external-model"));
    assert!(!text.contains("old-account-model"));
    assert!(!text.contains("expired-fixture-access"));
    let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
    for key in [
        "provider",
        "model",
        "reasoning_effort",
        "limits",
        "write",
        "image",
        "previous_run",
    ] {
        assert_eq!(saved[key], original[key]);
    }
    Ok(())
}

#[test]
fn explicit_reasoning_selection_persists_without_changing_saved_contracts() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let original = profile();
    fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
    let home = directory.path().join("codex-home");
    fs::create_dir(&home)?;
    fs::write(
        home.join("models_cache.json"),
        serde_json::to_vec(
            &json!({"models":[{"slug":"old-model","visibility":"list","supported_reasoning_levels":[{"effort":"low"},{"effort":"high"}]}]}),
        )?,
    )?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .env("CODEX_HOME", home)
        .env("HOME", directory.path())
        .env("USERPROFILE", directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/reasoning\n3\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
    assert_eq!(saved["reasoning_effort"], "high");
    for key in [
        "provider",
        "model",
        "endpoint",
        "limits",
        "write",
        "image",
        "previous_run",
    ] {
        assert_eq!(saved[key], original[key]);
    }
    assert!(String::from_utf8_lossy(&output.stdout).contains("reasoning high"));
    Ok(())
}

#[test]
fn reselecting_current_model_preserves_reasoning_and_new_model_resets_it() -> Result<()> {
    for (choice, expected) in [(1, Some("high")), (2, None)] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let mut original = profile();
        original["reasoning_effort"] = json!("high");
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let home = directory.path().join("codex-home");
        fs::create_dir(&home)?;
        fs::write(
            home.join("models_cache.json"),
            serde_json::to_vec(&json!({"models":[
                {"slug":"old-model","visibility":"list"},
                {"slug":"new-model","visibility":"list"}
            ]}))?,
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("CODEX_HOME", home)
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child
            .stdin
            .take()
            .unwrap()
            .write_all(format!("/model\n{choice}\n/quit\n").as_bytes())?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        assert_eq!(saved["reasoning_effort"].as_str(), expected);
        assert_eq!(
            saved["model"],
            if choice == 1 {
                "old-model"
            } else {
                "new-model"
            }
        );
        assert_eq!(saved["limits"], original["limits"]);
        assert_eq!(saved["previous_run"], original["previous_run"]);
    }
    Ok(())
}

#[test]
fn settings_change_only_the_selected_section_and_remove_unapproved_commands() -> Result<()> {
    for environment in [false, true] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let original = profile();
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("HOME", directory.path())
            .env("USERPROFILE", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(if environment {
            b"/settings\n1\n2\n/quit\n"
        } else {
            b"/settings\n2\n2\n/quit\n"
        })?;
        let output = child.wait_with_output()?;
        assert!(output.status.success());
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        for key in ["provider", "model", "endpoint", "previous_run"] {
            assert_eq!(saved[key], original[key]);
        }
        if environment {
            assert_eq!(saved["write"], false);
            assert!(saved["image"].is_null());
            assert_eq!(saved["limits"], original["limits"]);
        } else {
            assert_eq!(saved["limits"]["wall_seconds"], 3600);
            assert_eq!(saved["write"], original["write"]);
            assert_eq!(saved["image"], original["image"]);
        }
    }
    Ok(())
}

#[test]
fn model_and_provider_switches_keep_permissions_budgets_and_task_history() -> Result<()> {
    for provider_switch in [false, true] {
        let directory = tempfile::tempdir()?;
        let root = directory.path().join(".arun");
        fs::create_dir(&root)?;
        let original = profile();
        fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
        let codex_home = directory.path().join("codex-home");
        fs::create_dir(&codex_home)?;
        fs::write(
            codex_home.join("models_cache.json"),
            serde_json::to_vec(
                &json!({"models":[{"slug":"catalog-model","display_name":"Friendly model","visibility":"list"},{"slug":"internal-hidden","visibility":"hide"}]}),
            )?,
        )?;
        let home = directory.path().join("home");
        let vault = arun::auth_store::Vault::new(home.join(".aegis/auth"));
        let session = arun::auth_store::Session {
            provider: "grok".into(),
            access_token: "fixture-private-access".into(),
            refresh_token: None,
            account_id: None,
            expires_at: 2000000000,
        };
        vault.save(&session)?;
        arun::provider_catalog::save(
            &vault,
            arun::direct::Provider::Grok,
            &session.credentials()?,
            vec![arun::catalog::RemoteModel {
                id: "grok-example".into(),
                label: "Friendly Grok".into(),
                reasoning_levels: Vec::new(),
                default_reasoning: None,
            }],
            || false,
        )?;
        fs::create_dir_all(home.join(".grok"))?;
        fs::write(
            home.join(".grok/models_cache.json"),
            serde_json::to_vec(
                &json!({"models":{"grok-example":{"info":{"id":"grok-example", "name":"Friendly Grok", "hidden":false}}}}),
            )?,
        )?;
        let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
            .current_dir(directory.path())
            .env("CODEX_HOME", codex_home)
            .env("HOME", &home)
            .env("USERPROFILE", &home)
            .env("PATH", directory.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(if provider_switch {
            b"/provider\n4\n1\n/quit\n"
        } else {
            b"/models\n1\n/quit\n"
        })?;
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
        for key in ["write", "image", "limits", "previous_run"] {
            assert_eq!(saved[key], original[key]);
        }
        assert_eq!(
            saved["model"],
            if provider_switch {
                "grok-example"
            } else {
                "catalog-model"
            }
        );
        let text = String::from_utf8_lossy(&output.stdout);
        assert!(!text.contains("Allow workspace edits"));
        assert!(!text.contains("internal-hidden"));
        assert!(text.contains(if provider_switch {
            "Friendly Grok"
        } else {
            "Friendly model"
        }));
    }
    Ok(())
}

#[test]
fn custom_setup_lists_authenticated_endpoint_models_without_saving_the_key() -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let server = std::thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > Duration::from_secs(15) {
                        bail!("Model catalog was not requested");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer)?;
            if count == 0 || request.len() > 16 * 1024 {
                bail!("Incomplete catalog request");
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let headers = String::from_utf8(request)?.to_lowercase();
        assert!(headers.starts_with("get /v1/models "));
        assert!(headers.contains("authorization: bearer fixture-catalog-key"));
        let body = json!({"data":[{"id":"z-model"},{"id":"a-model"}]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(())
    });
    let directory = tempfile::tempdir()?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(
        format!("5\nhttp://{address}/v1\n1\nfixture-catalog-key\n2\n2\n/quit\n").as_bytes(),
    )?;
    let output = child.wait_with_output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("a-model") && text.contains("z-model"));
    assert!(!text.contains("fixture-catalog-key"));
    let saved = fs::read_to_string(directory.path().join(".arun/profile.json"))?;
    assert!(!saved.contains("fixture-catalog-key"));
    let saved: Value = serde_json::from_str(&saved)?;
    assert_eq!(saved["model"], "z-model");
    Ok(())
}

#[test]
fn settings_approve_a_keyless_local_fallback_from_its_live_catalog() -> Result<()> {
    approve_local_fallback(None)
}

#[test]
fn settings_approve_an_authenticated_local_fallback_without_persisting_the_key() -> Result<()> {
    approve_local_fallback(Some("private-fallback-fixture-key"))
}

fn approve_local_fallback(key: Option<&str>) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    listener.set_nonblocking(true)?;
    let expected_key = key.map(str::to_owned);
    let server = std::thread::spawn(move || -> Result<()> {
        let started = Instant::now();
        let mut stream = loop {
            match listener.accept() {
                Ok((stream, _)) => break stream,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if started.elapsed() > Duration::from_secs(15) {
                        bail!("Fallback catalog was not requested");
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => return Err(error.into()),
            }
        };
        stream.set_read_timeout(Some(Duration::from_secs(5)))?;
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer)?;
            if count == 0 || request.len() > 16 * 1024 {
                bail!("Incomplete fallback catalog request");
            }
            request.extend_from_slice(&buffer[..count]);
        }
        let headers = String::from_utf8(request)?.to_lowercase();
        assert!(headers.starts_with("get /v1/models "));
        if let Some(key) = expected_key {
            assert!(headers.contains(&format!("authorization: bearer {key}")));
        } else {
            assert!(!headers.contains("authorization:"));
        }
        let body = json!({"data":[{"id":"qwen-local"}]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )?;
        Ok(())
    });
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let original = profile();
    fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child.stdin.take().unwrap().write_all(
        format!(
            "/settings\n10\n3\nhttp://{address}/v1\n1\n{}\n1\n2\n/quit\n",
            key.unwrap_or_default()
        )
        .as_bytes(),
    )?;
    let output = child.wait_with_output()?;
    server.join().unwrap()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Context sharing"));
    assert!(text.contains("qwen-local"));
    if let Some(key) = key {
        assert!(!text.contains(key));
    }
    let profile_bytes = fs::read(root.join("profile.json"))?;
    if let Some(key) = key {
        assert!(!String::from_utf8_lossy(&profile_bytes).contains(key));
    }
    let saved: Value = serde_json::from_slice(&profile_bytes)?;
    assert_eq!(saved["fallback_routes"][0]["provider"], "custom");
    assert_eq!(saved["fallback_routes"][0]["model"], "qwen-local");
    assert_eq!(
        saved["fallback_routes"][0]["endpoint"]["base_url"],
        format!("http://{address}/v1")
    );
    assert_eq!(
        saved["fallback_routes"][0]["endpoint"]["api_key_env"],
        key.map_or(Value::Null, |_| json!("ARUN_SESSION_API_KEY"))
    );
    for key in [
        "provider",
        "model",
        "limits",
        "write",
        "image",
        "previous_run",
    ] {
        assert_eq!(saved[key], original[key]);
    }
    Ok(())
}

#[test]
fn fallback_menu_offers_only_remaining_provider_after_the_first_choice() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let mut original = profile();
    original["fallback_routes"] = json!([{"provider":"custom","model":"qwen-local","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":null}}]);
    let profile_bytes = serde_json::to_vec(&original)?;
    fs::write(root.join("profile.json"), &profile_bytes)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/settings\n10\n4\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(text.contains("Add Grok after current fallback"));
    assert!(!text.contains("Use Custom endpoint as fallback"));
    assert_eq!(fs::read(root.join("profile.json"))?, profile_bytes);
    Ok(())
}

#[test]
fn fallback_order_can_be_swapped_without_signing_in_or_losing_settings() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    fs::create_dir(&root)?;
    let mut original = profile();
    original["fallback_routes"] = json!([
        {"provider":"grok","model":"grok-model"},
        {"provider":"custom","model":"qwen-local","endpoint":{"base_url":"http://127.0.0.1:1234/v1","api_key_env":null}}
    ]);
    fs::write(root.join("profile.json"), serde_json::to_vec(&original)?)?;
    let mut child = Command::new(env!("CARGO_BIN_EXE_arun"))
        .current_dir(directory.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"/settings\n10\n2\n/quit\n")?;
    let output = child.wait_with_output()?;
    assert!(output.status.success());
    let saved: Value = serde_json::from_slice(&fs::read(root.join("profile.json"))?)?;
    assert_eq!(saved["fallback_routes"][0]["provider"], "custom");
    assert_eq!(saved["fallback_routes"][1]["provider"], "grok");
    for key in [
        "provider",
        "model",
        "write",
        "image",
        "limits",
        "previous_run",
    ] {
        assert_eq!(saved[key], original[key]);
    }
    Ok(())
}
