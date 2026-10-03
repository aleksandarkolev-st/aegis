use std::fs;

use anyhow::Result;
use arun::{storage::Store, worker};
use serde_json::json;
use sha2::{Digest, Sha256};

#[test]
fn large_requested_batches_paginate_actual_unicode_text_and_allow_distinct_file_ranges()
-> Result<()> {
    let directory = tempfile::tempdir()?;
    let text = "🦊".repeat(70_000);
    fs::write(directory.path().join("README.md"), &text)?;
    fs::write(directory.path().join("package.json"), "{}")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let execute = |store: &mut Store, arguments| -> Result<serde_json::Value> {
        let operation = store.begin_operation(&run.id, "workspace.read_batch", arguments, true)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        worker::execute(&root, &operation.id)
    };
    let result = execute(
        &mut store,
        json!({"files":[{"path":"README.md","length":65536},{"path":"package.json","length":4000}]}),
    )?;
    assert_eq!(
        result["selected"][0]["text"]
            .as_str()
            .unwrap()
            .chars()
            .count(),
        65_536
    );
    assert_eq!(result["selected"][0]["next_offset"], 65_536);
    assert_eq!(result["selected"][0]["total_characters"], 70_000);
    assert_eq!(result["selected"][1]["text"], "");
    assert_eq!(result["selected"][1]["next_offset"], 0);
    let rest = execute(
        &mut store,
        json!({"files":[{"path":"README.md","offset":65536,"length":65536},{"path":"package.json","length":4000}]}),
    )?;
    assert_eq!(rest["selected"][0]["next_offset"], 70_000);
    assert_eq!(rest["selected"][1]["text"], "{}");
    let ranges = execute(
        &mut store,
        json!({"files":[{"path":"README.md","offset":0,"length":1600},{"path":"README.md","offset":4000,"length":1600},{"path":"README.md","offset":8000,"length":1600}]}),
    )?;
    assert_eq!(ranges["selected"][2]["next_offset"], 9600);
    assert_eq!(ranges["selected"].as_array().unwrap().len(), 3);
    Ok(())
}

#[test]
fn file_scoped_search_excludes_other_files_and_checks_permissions_before_execution() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(
        directory.path().join("README.md"),
        "# Project\n## Install\n## Usage\n",
    )?;
    fs::write(directory.path().join("other.md"), "## Unrelated")?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "headings",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({"filesystem_scopes":{"read":["README.md"],"write":[]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let op = store.begin_operation(
        &run.id,
        "workspace.search",
        json!({"path":"README.md","query":"##"}),
        true,
    )?;
    store.operation_state(&op, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &op.id)?;
    assert_eq!(result["matches"].as_array().unwrap().len(), 2);
    assert!(
        result["matches"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["path"] == "README.md")
    );
    let denied = store.begin_operation(
        &run.id,
        "workspace.search",
        json!({"path":"other.md","query":"##"}),
        true,
    )?;
    store.operation_state(&denied, "dispatched", None, json!({}))?;
    assert!(worker::execute(&root, &denied.id).is_err());
    Ok(())
}

#[test]
fn selected_reads_are_bounded_unicode_exact_and_read_only() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let first = "αβγ🛡️\nfirst tail";
    fs::write(directory.path().join("first.txt"), first)?;
    fs::write(
        directory.path().join("second.txt"),
        "second selected\nother tail",
    )?;
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(&run.id, "workspace.read_batch", json!({"files":[{"path":"first.txt","offset":2,"length":5},{"path":"second.txt","length":15}]}), true)?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    let result = worker::execute(&root, &operation.id)?;
    assert_eq!(
        result["selected"][0]["text"],
        first.chars().skip(2).take(5).collect::<String>()
    );
    assert_eq!(result["selected"][0]["offset"], 2);
    assert_eq!(result["selected"][0]["next_offset"], 7);
    assert_eq!(
        result["selected"][0]["total_characters"],
        first.chars().count()
    );
    assert_eq!(
        result["selected"][0]["sha256"],
        hex::encode(Sha256::digest(first.as_bytes()))
    );
    assert_eq!(result["selected"][1]["text"], "second selected");
    assert_eq!(
        fs::read_to_string(directory.path().join("first.txt"))?,
        first
    );
    assert!(worker::execute(&root, &operation.id).is_err());
    assert_eq!(store.operation(&operation.id)?.state, "executing");
    Ok(())
}

#[test]
fn invalid_batches_and_denied_paths_cannot_claim_a_direct_worker() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("allowed.txt"), "allowed")?;
    fs::write(directory.path().join("secret.txt"), "secret")?;
    fs::create_dir(directory.path().join("folder"))?;
    fs::write(
        directory.path().join("large.txt"),
        vec![b'x'; 2 * 1024 * 1024 + 1],
    )?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({"filesystem_scopes":{"read":["allowed.txt"],"write":[]}}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let requests = [
        json!({"files":[]}),
        json!({"files":[{"path":"allowed.txt","length":1501},{"path":"secret.txt","length":1500}]}),
        json!({"files":[{"path":"allowed.txt","length":3},{"path":"secret.txt","length":3}]}),
        json!({"files":[{"path":"allowed.txt","length":3},{"path":"allowed.txt","length":3}]}),
        json!({"files":[{"path":"../allowed.txt","length":3}]}),
        json!({"files":[{"path":".arun/state.db","length":3}]}),
        json!({"files":[{"path":"allowed.txt","length":0}]}),
        json!({"files":[{"path":"allowed.txt","offset":-1,"length":3}]}),
        json!({"files":[{"path":"allowed.txt","length":3,"unapproved":true}]}),
        json!({"files":[{"path":"allowed.txt","length":3}],"unapproved":true}),
    ];
    for arguments in requests {
        let operation = store.begin_operation(&run.id, "workspace.read_batch", arguments, true)?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    }
    let broad = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    store.state(&broad.id, "running", json!({}))?;
    for path in ["folder", "large.txt", "missing.txt"] {
        let operation = store.begin_operation(
            &broad.id,
            "workspace.read_batch",
            json!({"files":[{"path":"allowed.txt","length":3},{"path":path,"length":3}]}),
            true,
        )?;
        store.operation_state(&operation, "dispatched", None, json!({}))?;
        assert!(worker::execute(&root, &operation.id).is_err());
        assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    }
    let denied = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!([]),
        json!({}),
        "",
    )?;
    store.state(&denied.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &denied.id,
        "workspace.read_batch",
        json!({"files":[{"path":"allowed.txt","length":3}]}),
        true,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    assert!(worker::execute(&root, &operation.id).is_err());
    assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    assert_eq!(
        fs::read_to_string(directory.path().join("secret.txt"))?,
        "secret"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn selected_reads_reject_links_even_without_narrow_scopes() -> Result<()> {
    let directory = tempfile::tempdir()?;
    fs::write(directory.path().join("actual.txt"), "actual")?;
    std::os::unix::fs::symlink("actual.txt", directory.path().join("link.txt"))?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "read",
        directory.path(),
        "fixture",
        json!(["workspace.read"]),
        json!({}),
        "",
    )?;
    store.state(&run.id, "running", json!({}))?;
    let operation = store.begin_operation(
        &run.id,
        "workspace.read_batch",
        json!({"files":[{"path":"link.txt","length":3}]}),
        true,
    )?;
    store.operation_state(&operation, "dispatched", None, json!({}))?;
    assert!(worker::execute(&root, &operation.id).is_err());
    assert_eq!(store.operation(&operation.id)?.state, "dispatched");
    Ok(())
}
