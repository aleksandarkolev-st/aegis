use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

type PairKey = (u64, String, String, u64);

fn interval(values: &[f64]) -> Value {
    if values.is_empty() {
        return json!({"pairs":0,"mean_delta":null,"bootstrap_95_percent_interval":null});
    }
    let mean = values.iter().sum::<f64>() / values.len() as f64;
    if values.len() < 2 {
        return json!({"pairs":values.len(),"mean_delta":mean,"bootstrap_95_percent_interval":null});
    }
    let mut seed = 0x49e6_f893_71ad_53c7_u64;
    let mut samples = Vec::with_capacity(10_000);
    for _ in 0..10_000 {
        let mut sum = 0.0;
        for _ in values {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            sum += values[seed as usize % values.len()];
        }
        samples.push(sum / values.len() as f64);
    }
    samples.sort_by(f64::total_cmp);
    json!({"pairs":values.len(),"mean_delta":mean,"bootstrap_95_percent_interval":[samples[249],samples[9749]]})
}

pub fn summarize(rows: &[Value]) -> Result<Value> {
    let mut conditions: BTreeMap<PairKey, BTreeMap<String, &Value>> = BTreeMap::new();
    let mut modes = BTreeSet::new();
    let mut experiment = None;
    for row in rows {
        let case = &row["case"];
        let size = case["size"].as_u64().context("case size missing")?;
        let task = case["task"].as_str().context("case task missing")?;
        let repeat = case["repeat"].as_u64().context("case repeat missing")?;
        let mode = case["mode"].as_str().context("case mode missing")?;
        let root = Path::new(case["root"].as_str().context("case root missing")?)
            .parent()
            .context("case experiment missing")?
            .to_path_buf();
        if experiment
            .as_ref()
            .is_some_and(|previous| previous != &root)
        {
            bail!("cannot pair records from different experiments");
        }
        experiment = Some(root);
        if row["acceptance"]["passed"].as_bool().is_none() {
            bail!("acceptance outcome missing");
        }
        let restart = case["restart_at"].as_str().unwrap_or("none");
        let group = conditions
            .entry((size, task.into(), restart.into(), repeat))
            .or_default();
        if group.insert(mode.into(), row).is_some() {
            bail!("duplicate experimental condition");
        }
        modes.insert(mode.to_owned());
    }
    let mut comparisons = Vec::new();
    for mode in modes.iter().filter(|mode| mode.as_str() != "eager") {
        let mut strata: BTreeMap<(u64, String, String), Vec<(&Value, &Value)>> = BTreeMap::new();
        let mut unmatched = 0;
        for ((size, task, restart, _), records) in &conditions {
            match (records.get("eager"), records.get(mode)) {
                (Some(baseline), Some(candidate)) => strata
                    .entry((*size, task.clone(), restart.clone()))
                    .or_default()
                    .push((baseline, candidate)),
                _ => unmatched += 1,
            }
        }
        let mut groups = Vec::new();
        for ((size, task, restart), pairs) in strata {
            let mut metrics = serde_json::Map::new();
            for (label, pointer) in [
                ("model_tokens", "/metrics/model_tokens"),
                ("recorded_model_tokens", "/metrics/model_tokens"),
                (
                    "unaccounted_model_attempts",
                    "/metrics/unaccounted_model_attempts",
                ),
                ("discovery_elapsed_ms", "/metrics/discovery_elapsed_ms"),
                ("execution_ms", "/execution_ms"),
                ("schema_bytes_peak", "/metrics/schema_bytes_peak"),
                ("wrong_tools", "/wrong_tools"),
                ("invalid_arguments", "/invalid_arguments"),
                ("repeated_dispatches", "/metrics/repeated_dispatches"),
                ("recovery_ms", "/restart/recovery_ms"),
            ] {
                let deltas: Vec<_> = pairs
                    .iter()
                    .filter_map(|(baseline, candidate)| {
                        if label == "discovery_elapsed_ms"
                            && [baseline, candidate].iter().any(|row| {
                                row["metrics"]["unaccounted_searches"].as_u64() != Some(0)
                            })
                        {
                            return None;
                        }
                        if label == "model_tokens"
                            && [baseline, candidate].iter().any(|row| {
                                row["metrics"]["unaccounted_model_attempts"].as_u64() != Some(0)
                                    || row["metrics"]["estimated_turns"].as_u64() != Some(0)
                            })
                        {
                            return None;
                        }
                        Some(
                            candidate.pointer(pointer)?.as_f64()?
                                - baseline.pointer(pointer)?.as_f64()?,
                        )
                    })
                    .collect();
                let mut result = interval(&deltas);
                result["excluded_pairs"] = json!(pairs.len() - deltas.len());
                metrics.insert(label.into(), result);
            }
            let acceptance: Vec<_> = pairs
                .iter()
                .map(|(baseline, candidate)| {
                    f64::from(candidate["acceptance"]["passed"].as_bool().unwrap())
                        - f64::from(baseline["acceptance"]["passed"].as_bool().unwrap())
                })
                .collect();
            let mean = acceptance.iter().sum::<f64>() / acceptance.len() as f64;
            let radius = (2.0 * (2.0_f64 / 0.05).ln() / acceptance.len() as f64).sqrt();
            groups.push(json!({"size":size,"task":task,"restart_at":restart,"pairs":pairs.len(),
                "acceptance":{"mean_delta":mean,"hoeffding_95_percent_interval":[(mean-radius).max(-1.0),(mean+radius).min(1.0)]},
                "both_accepted":pairs.iter().filter(|(baseline,candidate)| baseline["acceptance"]["passed"] == true && candidate["acceptance"]["passed"] == true).count(),
                "context_overflow":{"baseline":pairs.iter().filter(|(baseline,_)| baseline["context_overflow"] == true).count(), "candidate":pairs.iter().filter(|(_,candidate)| candidate["context_overflow"] == true).count()},
                "metrics":metrics}));
        }
        comparisons.push(json!({"baseline":"eager","candidate":mode,"unmatched_conditions":unmatched,"groups":groups}));
    }
    Ok(
        json!({"rows":rows.len(),"experiment":experiment,"comparisons":comparisons,
        "method":{"pairing":"same experiment, registry size, task, restart condition, and repeat", "direction":"candidate minus eager",
            "acceptance_interval":"distribution-free Hoeffding bound for independent paired repeats, difference bounded in [-1,1]",
            "numeric_interval":"deterministic paired percentile bootstrap; 10000 resamples; null for fewer than two pairs",
            "limitations":"Intervals assume independent repeats. Small samples, correlated provider sessions, and provider-default models limit inference. Recorded tokens are not billed cost. Complete model-token comparisons exclude pairs with estimated usage, unaccounted attempts, or missing accounting metadata; recorded_model_tokens retains known partial totals. Failed and overflow cases are not performance-gain claims. Missing metrics are excluded pairwise, with their counts reported. Repeated dispatches are not measured duplicate side effects."}}),
    )
}

pub fn from_file(path: &Path) -> Result<Value> {
    let file = File::open(path)?;
    if file.metadata()?.len() > 256 * 1024 * 1024 {
        bail!("evaluation report exceeds 256 MiB");
    }
    let mut rows = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        if line.len() > 2 * 1024 * 1024 {
            bail!("evaluation record exceeds 2 MiB");
        }
        if !line.trim().is_empty() {
            rows.push(serde_json::from_str(&line)?);
        }
    }
    summarize(&rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(mode: &str, repeat: u64, passed: bool, tokens: u64) -> Value {
        json!({"case":{"size":50,"task":"read","repeat":repeat,"mode":mode,"root":"experiment/registry-50"},"acceptance":{"passed":passed},"metrics":{"model_tokens":tokens,"unaccounted_model_attempts":0,"estimated_turns":0},"context_overflow":false})
    }

    #[test]
    fn pairs_repeats_instead_of_treating_individual_runs_as_independent() -> Result<()> {
        let rows = vec![
            row("durable", 1, true, 50),
            row("eager", 0, false, 100),
            row("durable", 0, true, 80),
            row("eager", 1, true, 100),
        ];
        let report = summarize(&rows)?;
        let group = &report["comparisons"][0]["groups"][0];
        assert_eq!(group["pairs"], 2);
        assert_eq!(group["acceptance"]["mean_delta"], 0.5);
        assert_eq!(group["metrics"]["model_tokens"]["mean_delta"], -35.0);
        assert_eq!(
            group["metrics"]["model_tokens"]["bootstrap_95_percent_interval"],
            json!([-50.0, -20.0])
        );
        assert_eq!(group["metrics"]["recovery_ms"]["pairs"], 0);
        assert_eq!(report, summarize(&rows)?);
        Ok(())
    }

    #[test]
    fn partial_unknown_or_estimated_usage_does_not_become_a_complete_token_comparison() -> Result<()>
    {
        let baseline = row("eager", 0, true, 100);
        for metadata in [json!(1), Value::Null] {
            let mut candidate = row("durable", 0, false, 20);
            candidate["metrics"]["unaccounted_model_attempts"] = metadata;
            let report = summarize(&[baseline.clone(), candidate])?;
            let metrics = &report["comparisons"][0]["groups"][0]["metrics"];
            assert_eq!(metrics["model_tokens"]["pairs"], 0);
            assert_eq!(metrics["model_tokens"]["excluded_pairs"], 1);
            assert_eq!(metrics["recorded_model_tokens"]["mean_delta"], -80.0);
        }
        let mut estimated = row("durable", 0, true, 80);
        estimated["metrics"]["estimated_turns"] = json!(1);
        let report = summarize(&[baseline, estimated])?;
        assert_eq!(
            report["comparisons"][0]["groups"][0]["metrics"]["model_tokens"]["pairs"],
            0
        );
        Ok(())
    }

    #[test]
    fn does_not_invent_precision_or_pair_unmatched_and_duplicate_records() -> Result<()> {
        let rows = vec![
            row("eager", 0, true, 100),
            row("durable", 0, true, 80),
            row("durable", 1, false, 20),
        ];
        let report = summarize(&rows)?;
        assert_eq!(report["comparisons"][0]["unmatched_conditions"], 1);
        assert_eq!(
            report["comparisons"][0]["groups"][0]["metrics"]["model_tokens"]["bootstrap_95_percent_interval"],
            Value::Null
        );
        assert!(summarize(&[rows[0].clone(), rows[0].clone()]).is_err());
        let mut foreign = rows[1].clone();
        foreign["case"]["root"] = json!("other/registry-50");
        assert!(summarize(&[rows[0].clone(), foreign]).is_err());
        Ok(())
    }
}
