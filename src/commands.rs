/// Commands exposed by the guided terminal, including aliases and contract subcommands.
pub const COMMANDS: &[(&str, &str)] = &[
    ("/goal", "View task or start from pasted text"),
    ("/contract", "Alias for /goal"),
    ("/goal add", "Review and add a requirement"),
    ("/goal replace", "Review a requirement replacement"),
    ("/goal history", "Inspect requirement changes"),
    ("/contract add", "Alias for /goal add"),
    ("/contract replace", "Alias for /goal replace"),
    ("/contract history", "Alias for /goal history"),
    ("/status", "Current task, progress and usage"),
    ("/why", "Show saved next action and blockers"),
    ("/evidence", "Inspect evidence; add O3 for a requirement"),
    ("/verify", "Show what prevents completion"),
    ("/provider", "Inspect the current task's provider"),
    ("/provider history", "Inspect provider transitions"),
    ("/budget", "Inspect usage and task time"),
    ("/handoff", "Inspect the next model's continuation state"),
    ("/pause", "Pause the selected task safely"),
    ("/resume", "Resume the selected saved task"),
    ("/providers", "Choose provider for new tasks"),
    ("/model", "Choose a model"),
    ("/models", "Alias for /model"),
    ("/reasoning", "Choose reasoning effort"),
    ("/login", "Sign in to the selected provider"),
    ("/settings", "Permissions, time and appearance"),
    ("/memory", "Review saved project notes"),
    ("/instructions", "Review workspace instructions"),
    ("/new", "Start a new conversation"),
    ("/sessions", "Select saved tasks and recovery"),
    ("/chats", "Alias for /sessions"),
    ("/context", "Inspect current context"),
    ("/tools", "Inspect granted tools"),
    ("/artifacts", "Browse saved artifacts"),
    ("/trace", "Inspect the task timeline"),
    ("/tasks", "Inspect plan milestones"),
    ("/checkpoint", "Inspect the saved checkpoint"),
    ("/cancel", "Review cancellation of the selected task"),
    ("/help", "Show keyboard controls and help"),
    ("/exit", "Exit this terminal session"),
    ("/quit", "Alias for /exit"),
];

#[cfg(test)]
mod tests {
    use super::COMMANDS;

    #[test]
    fn goal_and_contract_descriptions_fit_narrow_menu() {
        for command in ["/goal", "/contract"] {
            let description = COMMANDS
                .iter()
                .find(|(name, _)| *name == command)
                .expect("command must remain in the catalog")
                .1;
            assert!(
                description.chars().count() < 36,
                "{command} description must be shorter than 36 characters"
            );
        }
    }
}
