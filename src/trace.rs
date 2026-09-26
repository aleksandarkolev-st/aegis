use serde::Serialize;

use crate::storage::{Event, Run};

#[derive(Debug, Default, Serialize)]
pub struct Metrics {
    pub model_turns: usize,
    pub model_tokens: u64,
    pub estimated_turns: usize,
    pub schema_bytes_initial: u64,
    pub schema_bytes_peak: u64,
    pub schema_count_peak: u64,
    pub prompt_chars_peak: u64,
    pub model_elapsed_ms: u64,
    pub searches: usize,
    pub invocations: usize,
    pub succeeded: usize,
    pub rejected: usize,
    pub repeated_dispatches: usize,
    pub wall_seconds: i64,
}

pub fn metrics(events: &[Event]) -> Metrics {
    let mut summary = Metrics::default();
    let mut dispatched = std::collections::HashSet::new();
    let mut first_schema_seen = false;
    for event in events {
        let number = |key: &str| {
            event
                .payload
                .get(key)
                .and_then(|value| value.as_u64())
                .unwrap_or(0)
        };
        match event.kind.as_str() {
            "model.started" | "context.over_limit" => {
                let schemas = number("schema_bytes");
                if !first_schema_seen {
                    summary.schema_bytes_initial = schemas;
                    first_schema_seen = true;
                }
                summary.schema_bytes_peak = summary.schema_bytes_peak.max(schemas);
                summary.schema_count_peak = summary.schema_count_peak.max(number("schema_count"));
                summary.prompt_chars_peak = summary.prompt_chars_peak.max(number("prompt_chars"));
            }
            "model.response" => {
                summary.model_turns += 1;
                summary.model_tokens +=
                    event.payload["usage"]["input_tokens"].as_u64().unwrap_or(0)
                        + event.payload["usage"]["output_tokens"]
                            .as_u64()
                            .unwrap_or(0);
                summary.estimated_turns +=
                    usize::from(event.payload["usage"]["source"] != "provider");
                summary.model_elapsed_ms += number("elapsed_ms");
            }
            "capability.search" => summary.searches += 1,
            "operation.pending" => summary.invocations += 1,
            "operation.dispatched" => {
                if let Some(id) = event.payload["id"].as_str() {
                    if !dispatched.insert(id.to_owned()) {
                        summary.repeated_dispatches += 1;
                    }
                }
            }
            "operation.succeeded" => summary.succeeded += 1,
            "action.rejected" => summary.rejected += 1,
            _ => {}
        }
    }
    if let (Some(first), Some(last)) = (events.first(), events.last()) {
        summary.wall_seconds = (last.created_at - first.created_at).max(0);
    }
    summary
}

pub fn display(run: &Run, events: &[Event]) {
    println!(
        "run {}  {}  {}  {}",
        run.id, run.state, run.provider, run.task
    );
    for event in events {
        match event.kind.as_str() {
            "model.started" => println!(
                "{:>4} model turn {}  schemas {} ({} bytes)  prompt {} chars",
                event.seq,
                event.payload["turn"],
                event.payload["schema_count"],
                event.payload["schema_bytes"],
                event.payload["prompt_chars"]
            ),
            "model.response" => println!(
                "{:>4} model {:?}  {} ms  {} input + {} output tokens",
                event.seq,
                event.payload["action"]["kind"],
                event.payload["elapsed_ms"],
                event.payload["usage"]["input_tokens"],
                event.payload["usage"]["output_tokens"]
            ),
            "capability.search" => println!(
                "{:>4} search {:?} -> {}",
                event.seq, event.payload["query"], event.payload["matches"]
            ),
            kind if kind.starts_with("operation.") => println!(
                "{:>4} {} {}  {}",
                event.seq, kind, event.payload["id"], event.payload["artifact"]
            ),
            kind if kind.starts_with("run.") || kind == "action.rejected" => {
                println!("{:>4} {} {}", event.seq, kind, event.payload)
            }
            _ => {}
        }
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&metrics(events)).unwrap_or_default()
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn aggregates_committed_turns_and_schema_exposure() {
        let events = vec![
            Event {
                seq: 1,
                kind: "model.started".into(),
                payload: json!({"schema_bytes": 300, "schema_count": 2, "prompt_chars": 900}),
                created_at: 10,
            },
            Event {
                seq: 2,
                kind: "model.response".into(),
                payload: json!({"usage":{"input_tokens": 40, "output_tokens": 5, "source":"provider"}, "elapsed_ms": 250}),
                created_at: 12,
            },
            Event {
                seq: 3,
                kind: "capability.search".into(),
                payload: json!({}),
                created_at: 13,
            },
            Event {
                seq: 4,
                kind: "model.started".into(),
                payload: json!({"schema_bytes": 600, "schema_count": 4, "prompt_chars": 1500}),
                created_at: 14,
            },
        ];
        let result = metrics(&events);
        assert_eq!(result.model_tokens, 45);
        assert_eq!(result.schema_bytes_initial, 300);
        assert_eq!(result.schema_bytes_peak, 600);
        assert_eq!(result.schema_count_peak, 4);
        assert_eq!(result.searches, 1);
        assert_eq!(result.wall_seconds, 4);
    }
}
