use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    kernel,
    storage::{Operation, Run, Store},
};
use serde_json::{Value, json};

pub(crate) const CAPABILITY: &str = "runtime.acceptance";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proposal {
    summary: String,
    evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Check {
    pub name: String,
    pub program: String,
    pub args: Vec<String>,
    pub image: String,
    #[serde(default = "default_seconds")]
    pub seconds: u64,
}

pub(crate) fn propose(
    store: &mut Store,
    run: &Run,
    summary: &str,
    evidence: &[String],
) -> Result<()> {
    store.validate_completion(&run.id, evidence)?;
    if summary.len() > 64_000 || evidence.len() > 100 {
        bail!("completion proposal exceeds limits");
    }
    let artifact = store.put_artifact(&serde_json::to_vec(&Proposal {
        summary: summary.into(),
        evidence: evidence.to_vec(),
    })?)?;
    store.event(&run.id, "completion.proposed", json!({"artifact":artifact}))
}

pub(crate) fn authorized_check(store: &Store, run: &Run, operation: &Operation) -> Result<Check> {
    let check =
        Check::from_run(run)?.context("no acceptance check was authorized for this task")?;
    let proposal = operation
        .arguments
        .get("proposal")
        .and_then(Value::as_str)
        .context("acceptance proposal missing")?;
    if operation.capability != CAPABILITY
        || operation.capability_version != check.version()?
        || !operation.retry_safe
        || operation.arguments != json!({"proposal":proposal})
        || store.completion_proposal(&run.id)?.as_deref() != Some(proposal)
    {
        bail!("acceptance operation does not match the pending approved completion");
    }
    let candidate: Proposal = serde_json::from_slice(&store.artifact(proposal)?)?;
    store.validate_completion_except_operation(
        &run.id,
        &candidate.evidence,
        Some(&operation.id),
    )?;
    Ok(check)
}

pub(crate) fn verified_result(
    store: &Store,
    run: &Run,
    summary: &str,
    evidence: &[String],
) -> Result<Option<String>> {
    if Check::from_run(run)?.is_none() {
        return Ok(None);
    }
    let proposal = store
        .completion_proposal(&run.id)?
        .context("independent acceptance is required before completion")?;
    let candidate: Proposal = serde_json::from_slice(&store.artifact(&proposal)?)?;
    if candidate.summary != summary || candidate.evidence != evidence {
        bail!("completion differs from the independently checked proposal");
    }
    let operation = store
        .operations(&run.id)?
        .into_iter()
        .rev()
        .find(|operation| {
            operation.capability == CAPABILITY && operation.arguments["proposal"] == proposal
        })
        .context("acceptance operation has not executed")?;
    authorized_check(store, run, &operation)?;
    let hash = operation
        .artifact
        .context("acceptance result has not been recorded")?;
    let result: Value = serde_json::from_slice(&store.artifact(&hash)?)?;
    if operation.state != "succeeded" || result["exit_code"] != 0 {
        bail!("independent acceptance check has not passed");
    }
    Ok(Some(hash))
}

pub(crate) fn resume(store: &mut Store, root: &Path, run: &Run) -> Result<bool> {
    let Some(check) = Check::from_run(run)? else {
        return Ok(false);
    };
    let Some(proposal) = store.completion_proposal(&run.id)? else {
        return Ok(false);
    };
    let candidate: Proposal = serde_json::from_slice(&store.artifact(&proposal)?)?;
    store.validate_completion(&run.id, &candidate.evidence)?;
    let elapsed = store
        .run_started_at(&run.id)?
        .map(|started| crate::storage::unix_time().saturating_sub(started) as u64)
        .unwrap_or(0);
    let remaining = run.budgets["wall_seconds"]
        .as_u64()
        .unwrap_or(3600)
        .saturating_sub(elapsed);
    let existing = store
        .operations(&run.id)?
        .into_iter()
        .rev()
        .find(|operation| {
            operation.capability == CAPABILITY && operation.arguments["proposal"] == proposal
        });
    let operation = match existing {
        Some(operation) => operation,
        None => store.begin_operation_versioned(
            &run.id,
            CAPABILITY,
            check.version()?,
            json!({"proposal":proposal}),
            true,
        )?,
    };
    if operation.state != "succeeded" {
        if remaining == 0 {
            store.state(
                &run.id,
                "waiting_recovery",
                json!({"reason":"wall-clock budget exhausted before acceptance"}),
            )?;
            return Ok(true);
        }
        authorized_check(store, run, &operation)?;
        kernel::cleanup_container(&operation);
        store.event(
            &run.id,
            "acceptance.started",
            json!({"name":check.name,"operation":operation.id,"proposal":proposal}),
        )?;
        let mut bounded = run.clone();
        bounded.budgets["process_seconds"] = json!(check.seconds.min(remaining));
        kernel::perform(store, root, &bounded, &operation)?;
    }
    if store.run(&run.id)?.state == "cancelled" {
        return Ok(true);
    }
    let operation = store.operation(&operation.id)?;
    if operation.state == "succeeded" {
        let artifact = operation
            .artifact
            .as_ref()
            .context("acceptance result missing")?;
        let result: Value = serde_json::from_slice(&store.artifact(artifact)?)?;
        if result["exit_code"] == 0 {
            store.event(
                &run.id,
                "acceptance.passed",
                json!({"name":check.name,"artifact":artifact,"proposal":proposal}),
            )?;
            store.complete_run(&run.id, &candidate.summary, &candidate.evidence)?;
            return Ok(true);
        }
        let excerpt = result["output_artifact"]
            .as_str()
            .map(|hash| store.artifact(hash))
            .transpose()?
            .map(|bytes| {
                String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(1500)
                    .collect::<String>()
            })
            .unwrap_or_default();
        store.event(&run.id, "acceptance.failed", json!({"name":check.name,"artifact":artifact,"exit_code":result["exit_code"],"excerpt":excerpt}))?;
        store.event(
            &run.id,
            "completion.resolved",
            json!({"proposal":proposal,"outcome":"rejected"}),
        )?;
        return Ok(false);
    }
    store.event(
        &run.id,
        "acceptance.unavailable",
        json!({"name":check.name,"operation":operation.id}),
    )?;
    store.state(
        &run.id,
        "waiting_recovery",
        json!({"reason":"acceptance check unavailable; resume after fixing its environment"}),
    )?;
    Ok(true)
}

fn default_seconds() -> u64 {
    30
}

impl Check {
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() || self.name.len() > 500 {
            bail!("acceptance check needs a name of at most 500 bytes");
        }
        if self.program.is_empty()
            || self.program.len() > 100
            || !self.program.as_bytes()[0].is_ascii_alphanumeric()
            || !self
                .program
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        {
            bail!("acceptance program must be a container executable name, not a path or option");
        }
        if self.image.is_empty()
            || self.image.len() > 256
            || self.image.starts_with('-')
            || !self
                .image
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"._/:@-".contains(&byte))
        {
            bail!("acceptance requires a valid locally available container image");
        }
        if !(1..=300).contains(&self.seconds)
            || self.args.len() > 64
            || self.args.iter().map(String::len).sum::<usize>() > 60_000
            || self.args.iter().any(|argument| argument.contains('\0'))
        {
            bail!("acceptance arguments or deadline exceed limits");
        }
        Ok(())
    }

    pub fn from_file(path: &Path) -> Result<Self> {
        let mut bytes = Vec::new();
        fs::File::open(path)?.take(65_537).read_to_end(&mut bytes)?;
        if bytes.len() > 65_536 {
            bail!("acceptance configuration exceeds 64 KiB");
        }
        let check: Self = serde_json::from_slice(&bytes)
            .context("acceptance file must contain a JSON check configuration")?;
        check.validate()?;
        Ok(check)
    }

    pub fn from_run(run: &Run) -> Result<Option<Self>> {
        let Some(value) = run
            .budgets
            .get("acceptance_check")
            .filter(|value| !value.is_null())
        else {
            return Ok(None);
        };
        let check: Self =
            serde_json::from_value(value.clone()).context("invalid acceptance configuration")?;
        check.validate()?;
        Ok(Some(check))
    }

    pub fn version(&self) -> Result<u32> {
        let hash = Sha256::digest(serde_json::to_vec(self)?);
        Ok(u32::from_be_bytes(hash[..4].try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freezes_validated_configurations_and_rejects_host_execution() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("acceptance.json");
        fs::write(
            &path,
            r#"{"name":"addition assertions","program":"node","args":["--input-type=module","-e","console.log('verified')"],"image":"node:22-alpine"}"#,
        )?;
        let check = Check::from_file(&path)?;
        assert_eq!(check.seconds, 30);
        let version = check.version()?;
        fs::write(&path, "not a configuration")?;
        assert_eq!(check.version()?, version);
        for program in ["/bin/sh", "../node", "--privileged", "node;echo"] {
            let mut invalid = check.clone();
            invalid.program = program.into();
            assert!(invalid.validate().is_err());
        }
        let mut changed = check;
        changed.args.push("different assertion".into());
        assert_ne!(changed.version()?, version);
        changed.seconds = 301;
        assert!(changed.validate().is_err());
        Ok(())
    }

    #[test]
    fn completion_cannot_bypass_verification_or_reuse_a_different_proposal() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let mut store = Store::open(directory.path())?;
        let check = Check {
            name: "test".into(),
            program: "node".into(),
            args: vec!["-e".into(), "process.exit(0)".into()],
            image: "node:22-alpine".into(),
            seconds: 30,
        };
        let run = store.create_run(
            "repair",
            directory.path(),
            "unused",
            json!([]),
            json!({"acceptance_check":check}),
            "test must pass",
        )?;
        store.state(&run.id, "running", json!({}))?;
        let read =
            store.begin_operation(&run.id, "workspace.read", json!({"path":"fixture"}), true)?;
        let evidence = store.put_artifact(b"fixture evidence")?;
        store.operation_state(&read, "succeeded", Some(&evidence), json!({}))?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        propose(&mut store, &run, "done", &[evidence.clone()])?;
        let proposal = store.completion_proposal(&run.id)?.unwrap();
        let operation = store.begin_operation_versioned(
            &run.id,
            CAPABILITY,
            check.version()?,
            json!({"proposal":proposal}),
            true,
        )?;
        assert!(authorized_check(&store, &run, &operation).is_ok());
        let unrelated = store.begin_operation(&run.id, "workspace.read", json!({}), true)?;
        assert!(authorized_check(&store, &run, &operation).is_err());
        store.operation_state(&unrelated, "cancelled", None, json!({}))?;
        assert!(authorized_check(&store, &run, &operation).is_ok());
        let failed = store.put_artifact(br#"{"exit_code":1}"#)?;
        store.operation_state(&operation, "succeeded", Some(&failed), json!({}))?;
        assert!(
            store
                .complete_run(&run.id, "done", &[evidence.clone()])
                .is_err()
        );
        let passed = store.put_artifact(br#"{"exit_code":0}"#)?;
        store.operation_state(&operation, "succeeded", Some(&passed), json!({}))?;
        assert!(
            store
                .complete_run(&run.id, "different summary", &[evidence.clone()])
                .is_err()
        );
        assert!(resume(&mut store, directory.path(), &run)?);
        assert_eq!(store.run(&run.id)?.state, "completed");
        assert_eq!(store.event_count(&run.id, "operation.dispatched")?, 0);
        assert!(store.completion_proposal(&run.id)?.is_none());
        Ok(())
    }
}
