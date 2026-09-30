use crate::routing::Route;

pub fn is_question(task: &str) -> bool {
    let normalized = task.trim().trim_end_matches(['?', '!', '.']).to_lowercase();
    let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
    matches!(
        normalized.as_str(),
        "what model are u"
            | "what model are you"
            | "which model are you"
            | "what model is this"
            | "what model are you using"
            | "which model is running"
            | "who are you"
            | "are you codex"
            | "are u codex"
    )
}

pub fn describe(route: &Route) -> String {
    let provider = match crate::provider::canonical(&route.provider) {
        "codex" => "ChatGPT",
        "grok" => "Grok",
        "claude-api" => "Claude API",
        "custom" => "Custom endpoint",
        other => other,
    };
    format!(
        "I'm Aegis. Current model: {}. Provider: {}. Reasoning: {}. Aegis manages the agent loop and tools.",
        route.model,
        provider,
        route
            .reasoning_effort
            .as_deref()
            .unwrap_or("provider default")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identity_questions_do_not_match_requests_to_do_work() {
        assert!(is_question(" What model are u? "));
        assert!(is_question("which model are you"));
        assert!(!is_question(
            "Explain what model are you using and refactor the parser"
        ));
        assert!(!is_question("Requirements:\n- who are you"));
    }
}
