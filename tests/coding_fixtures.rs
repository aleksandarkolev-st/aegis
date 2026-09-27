use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde_json::Value;

#[test]
fn independent_graders_report_named_cases_and_reject_broken_starters_without_agents() -> Result<()>
{
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("benchmarks/coding");
    for task in [
        "json-patch",
        "dag-scheduler",
        "sse-decoder",
        "interval-overlay",
    ] {
        let output = Command::new("node")
            .arg(root.join("grade.mjs"))
            .arg(task)
            .arg(root.join(format!("{task}.mjs")))
            .output()?;
        assert_eq!(
            output.status.code(),
            Some(1),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let result: Value = serde_json::from_slice(&output.stdout)?;
        assert_eq!(result["task"], task);
        assert_eq!(result["passed"], false);
        assert!(
            result["total_cases"]
                .as_u64()
                .is_some_and(|count| count >= 5)
        );
        assert!(
            result["cases"]
                .as_array()
                .unwrap()
                .iter()
                .any(|case| case["passed"] == false)
        );
    }
    Ok(())
}
