use std::path::PathBuf;

use anyhow::Result;
use arun::{capability, mcp, storage::Store};
use serde_json::json;

#[test]
fn evaluation_registries_have_exact_sizes_and_large_log_results() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/eval.mjs");
    for size in [50, 100, 250, 500] {
        let server = mcp::Server {
            policy: mcp::Policy {
                trusted_host: true,
                ..Default::default()
            },
            name: "eval".into(),
            command: "node".into(),
            args: vec![
                fixture.to_string_lossy().into_owned(),
                (size - 4).to_string(),
            ],
        };
        let tools = mcp::discover(&server, directory.path())?;
        let root = directory.path().join(size.to_string());
        let mut store = Store::open(&root)?;
        store.register_mcp(&server, &tools)?;
        assert_eq!(capability::all(&store)?.len(), size);
        let result = mcp::call(
            &server,
            directory.path(),
            "build_log",
            json!({"query":"error"}),
        )?;
        assert!(result.to_string().len() > 2_000_000);
        assert!(result.to_string().contains("AEGIS_EVAL_LOG_FAILURE"));
    }
    Ok(())
}
