use anyhow::Result;
use arun::storage::Store;
use serde_json::json;

#[test]
fn planned_exposures_are_metered_without_a_task_cap_and_survive_archiving() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let root = directory.path().join(".arun");
    let mut store = Store::open(&root)?;
    let run = store.create_run(
        "inspect",
        directory.path(),
        "fixture",
        json!([]),
        json!({"mode":"durable","tool_result_tokens":12}),
        "",
    )?;
    assert_eq!(run.budgets["context_tokenizer"], "o200k_base");
    assert!(run.budgets.get("tool_result_tokens").is_none());
    let measured = json!({"context_tokenizer":"o200k_base","tool_result_tokens":6,"schema_tokens":10,"raw_prompt_tokens":30});
    store.event(&run.id, "model.started", measured.clone())?;
    assert_eq!(store.tool_result_tokens(&run.id)?, 6);
    store.save_snapshot(&run.id)?;
    drop(store);
    let mut store = Store::open(&root)?;
    store.event(&run.id, "model.started", measured.clone())?;
    assert_eq!(store.tool_result_tokens(&run.id)?, 12);
    store.event(&run.id, "model.started", measured)?;
    assert_eq!(store.tool_result_tokens(&run.id)?, 18);
    assert!(store.load_recovery(&run.id)?.is_some());
    for _ in 0..1100 {
        store.event(&run.id, "fixture.padding", json!({}))?;
    }
    store.save_snapshot(&run.id)?;
    assert!(store.archive_history(&run.id)? > 0);
    assert!(store.load_recovery(&run.id)?.is_some());
    let metrics = arun::trace::metrics(&store.events(&run.id)?);
    assert_eq!(metrics.tool_result_tokens, 18);
    assert_eq!(metrics.schema_tokens_initial, Some(10));
    assert_eq!(metrics.raw_prompt_tokens, 90);
    assert_eq!(metrics.unaccounted_context_attempts, 0);
    Ok(())
}
