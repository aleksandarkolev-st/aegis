use aegis_relay::model::{AgentCommand, CommandParseError, parse_command};

#[path = "../../src/commands.rs"]
mod terminal_command_catalog;

#[test]
fn phone_control_tokens_accept_bare_and_slash_forms() {
    for input in ["tasks", "/tasks", "/list_tasks", " \t tasks \r\n"] {
        assert_eq!(
            parse_command(input),
            Ok(AgentCommand::ListTasks),
            "{input:?}"
        );
    }

    for input in ["details", "/details", " details\t "] {
        assert_eq!(
            parse_command(input),
            Ok(AgentCommand::Details { task_id: None }),
            "{input:?}"
        );
    }

    for prefix in ["details", "/details"] {
        assert_eq!(
            parse_command(&format!("{prefix}\t t-abcdef")),
            Ok(AgentCommand::Details {
                task_id: Some("t-abcdef".into())
            })
        );
    }

    for prefix in ["use", "/use", "/select_task"] {
        for alias in ["t-abcdef", "task-4", "my_task.1", "A"] {
            assert_eq!(
                parse_command(&format!("{prefix} {alias}")),
                Ok(AgentCommand::SelectTask {
                    task_id: alias.into()
                })
            );
        }
    }
}

#[test]
fn phone_control_tokens_reject_missing_malformed_and_extra_arguments() {
    for input in [
        "tasks extra",
        "/list_tasks extra",
        "use",
        "/use",
        "/select_task",
        "use task-1 extra",
        "/use task-1 extra",
        "/select_task task-1 extra",
        "details task-1 extra",
        "/details task-1 extra",
    ] {
        assert_eq!(
            parse_command(input),
            Err(CommandParseError::InvalidSyntax),
            "{input:?}"
        );
    }

    for prefix in ["use", "/use", "/select_task", "details", "/details"] {
        for alias in ["task/1", "task:1", "task@1", "\u{0430}", &"a".repeat(129)] {
            let input = format!("{prefix} {alias}");
            assert_eq!(
                parse_command(&input),
                Err(CommandParseError::InvalidSyntax),
                "{input:?}"
            );
        }
        assert!(parse_command(&format!("{prefix} {}", "a".repeat(128))).is_ok());
    }
}

#[test]
fn phone_control_tokens_do_not_match_inside_prose_or_longer_words() {
    for input in [
        "I have tasks to do",
        "Please use task-1",
        "Show me details",
        "tasksome work",
        "useful information",
        "detailsome information",
        "tasks?",
        "use: task-1",
        "details!",
        "Tasks",
        "status",
        "approve_once challenge-1",
    ] {
        assert_eq!(
            parse_command(input),
            Ok(AgentCommand::Message { text: input.into() }),
            "{input:?}"
        );
    }
}

#[test]
fn every_terminal_catalog_command_remains_a_remote_slash_command() {
    for (command, _) in terminal_command_catalog::COMMANDS {
        let parsed = parse_command(command).unwrap_or_else(|error| {
            panic!("catalog command {command:?} failed to parse: {error}")
        });
        assert!(
            !matches!(parsed, AgentCommand::Message { .. }),
            "catalog command {command:?} must not be reinterpreted as model task text"
        );
    }
}

#[test]
fn unknown_slash_commands_are_forwarded_without_becoming_model_messages() {
    for input in [
        "/unknown-command",
        "/tasks unexpected",
        "/tasks-extra",
        "/useful task-1",
        "/details-extra",
        "/confirm O3",
        "/back",
    ] {
        assert_eq!(
            parse_command(input),
            Ok(AgentCommand::Slash { text: input.into() }),
            "{input:?} must stay on the slash-command path"
        );
    }
}
