use serde::Serialize;

use crate::storage::{Event, Run};

#[derive(Debug, Default, Serialize)]
pub struct Metrics {
    pub model_attempts: usize,
    pub model_turns: usize,
    pub failed_model_turns: usize,
    pub unaccounted_model_attempts: usize,
    pub model_tokens: u64,
    pub estimated_turns: usize,
    pub schema_bytes_initial: u64,
    pub schema_bytes_peak: u64,
    pub schema_count_peak: u64,
    pub context_tokenizer: Option<String>,
    pub schema_tokens_initial: Option<u64>,
    pub schema_tokens_peak: Option<u64>,
    pub tool_result_tokens: u64,
    pub raw_prompt_tokens: u64,
    pub unaccounted_context_attempts: usize,
    pub prompt_chars_peak: u64,
    pub model_elapsed_ms: u64,
    pub searches: usize,
    pub discovery_elapsed_ms: u64,
    pub unaccounted_searches: usize,
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
    let mut missing_response_usage = 0;
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
                summary.model_attempts += usize::from(event.kind == "model.started");
                if event.kind == "model.started" {
                    if event.payload["context_tokenizer"] == crate::tokenization::ENCODING
                        && ["schema_tokens", "tool_result_tokens", "raw_prompt_tokens"]
                            .iter()
                            .all(|key| event.payload[*key].as_u64().is_some())
                    {
                        summary.context_tokenizer = Some(crate::tokenization::ENCODING.into());
                        let tokens = number("schema_tokens");
                        if summary.model_attempts == 1 {
                            summary.schema_tokens_initial = Some(tokens);
                        }
                        summary.schema_tokens_peak =
                            Some(summary.schema_tokens_peak.unwrap_or(0).max(tokens));
                        summary.tool_result_tokens = summary
                            .tool_result_tokens
                            .saturating_add(number("tool_result_tokens"));
                        summary.raw_prompt_tokens = summary
                            .raw_prompt_tokens
                            .saturating_add(number("raw_prompt_tokens"));
                    } else {
                        summary.unaccounted_context_attempts += 1;
                    }
                }
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
                let input = event.payload["usage"]["input_tokens"].as_u64();
                let output = event.payload["usage"]["output_tokens"].as_u64();
                missing_response_usage += usize::from(input.is_none() || output.is_none());
                summary.model_tokens = summary
                    .model_tokens
                    .saturating_add(input.unwrap_or(0))
                    .saturating_add(output.unwrap_or(0));
                summary.estimated_turns +=
                    usize::from(event.payload["usage"]["source"] != "provider");
                summary.model_elapsed_ms = summary
                    .model_elapsed_ms
                    .saturating_add(number("elapsed_ms"));
            }
            "model.failed" => {
                summary.failed_model_turns += 1;
                summary.model_elapsed_ms = summary
                    .model_elapsed_ms
                    .saturating_add(number("elapsed_ms"));
            }
            "capability.search" => {
                summary.searches += 1;
                summary.unaccounted_searches +=
                    usize::from(event.payload["elapsed_ms"].as_u64().is_none());
                summary.discovery_elapsed_ms = summary
                    .discovery_elapsed_ms
                    .saturating_add(number("elapsed_ms"));
            }
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
    summary.unaccounted_model_attempts =
        summary.model_attempts.saturating_sub(summary.model_turns) + missing_response_usage;
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
        assert_eq!(result.unaccounted_searches, 1);
        assert_eq!(result.model_attempts, 2);
        assert_eq!(result.unaccounted_model_attempts, 1);
        assert_eq!(result.unaccounted_context_attempts, 2);
        assert_eq!(result.context_tokenizer, None);
        assert_eq!(result.schema_tokens_initial, None);
        assert_eq!(result.schema_tokens_peak, None);
        assert_eq!(result.wall_seconds, 4);
    }

    #[test]
    fn failed_and_missing_usage_attempts_are_not_reported_as_zero_cost() {
        let event = |seq, kind: &str, payload| Event {
            seq,
            kind: kind.into(),
            payload,
            created_at: seq,
        };
        let events = vec![
            event(1, "model.started", json!({})),
            event(2, "model.failed", json!({"elapsed_ms":1200})),
            event(3, "model.started", json!({})),
            event(4, "model.response", json!({"usage":null,"elapsed_ms":200})),
            event(5, "capability.search", json!({"elapsed_ms":7})),
            event(6, "context.over_limit", json!({})),
        ];
        let result = metrics(&events);
        assert_eq!(result.model_attempts, 2);
        assert_eq!(result.model_turns, 1);
        assert_eq!(result.failed_model_turns, 1);
        assert_eq!(result.unaccounted_model_attempts, 2);
        assert_eq!(result.model_elapsed_ms, 1400);
        assert_eq!(result.discovery_elapsed_ms, 7);
        assert_eq!(result.unaccounted_searches, 0);
    }
}
