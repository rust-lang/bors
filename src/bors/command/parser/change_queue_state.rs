use crate::bors::RollupMode;
use crate::bors::command::parser::{CommandPart, ParseResult, join_parts, parser_squash};
use crate::bors::command::{
    Approval, Approver, BorsCommand, CommandParseError, Priority, QueueProperties,
};
use crate::database::ApprovalMode;
use std::str::FromStr;

/// Parses:
/// - "@bors r+ [p=<priority>] [rollup=<never|iffy|maybe|always>] [force] [note=<note>]"
/// - "@bors r=<user> [p=<priority>] [rollup=<never|iffy|maybe|always>] [force] [note=<note>]"
pub(super) fn parser_change_queue_state(
    command: &CommandPart<'_>,
    parts: &[CommandPart<'_>],
) -> ParseResult {
    let also_squash = parts
        .iter()
        .position(|x| *x == CommandPart::Bare("squash"))
        .map(|x| &parts[x..]);

    let approver = match command {
        CommandPart::Bare("r+") => Approver::Myself,
        CommandPart::KeyValue { key: "r", value } => {
            if value.is_empty() {
                return Some(Err(CommandParseError::MissingArgValue {
                    arg: "r".to_string(),
                }));
            }
            Approver::Specified(value.to_string())
        }
        _ => return None,
    };

    let priority = match parse_priority(parts) {
        Some(Ok(p)) => Some(p),
        Some(Err(e)) => return Some(Err(e)),
        None => None,
    };
    let rollup = match parse_rollup(parts) {
        Some(Ok(p)) => Some(p),
        Some(Err(e)) => return Some(Err(e)),
        None => None,
    };
    let note = parts
        .iter()
        .filter_map(|part| match part {
            CommandPart::KeyValue { key: "note", value } => Some(value.to_string()),
            _ => None,
        })
        .next();
    let force = parts
        .iter()
        .any(|part| matches!(part, CommandPart::Bare("force")));
    let mode = if force {
        ApprovalMode::Eager
    } else {
        ApprovalMode::Tentative
    };

    // parse the squash part of the approve_squash command
    if let Some(also_squash) = also_squash {
        let filter_priority = ["r", "rollup", "p", "priority", "force", "note"];

        let also_squash: Vec<CommandPart> = also_squash
            .iter()
            .filter(|x| !filter_priority.contains(&x.as_key()))
            .copied()
            .collect();

        let val = parser_squash(&also_squash[0], &also_squash[1..]).unwrap();

        match val {
            Ok(val) => match val {
                BorsCommand::Squash { commit_message, .. } => {
                    return Some(Ok(BorsCommand::SquashApprove {
                        commit_message,
                        approval_info: QueueProperties {
                            approval: Some(Approval { approver, mode }),
                            priority,
                            rollup,
                            note,
                        },
                    }));
                }
                _ => unreachable!(),
            },
            Err(_) => {
                return Some(Err(CommandParseError::UnknownArg {
                    arg: also_squash[1].as_key().to_owned(),
                    did_you_mean: "r+ squash [msg|message=\"<commit-msg>\"|description]"
                        .to_string(),
                }));
            }
        }
    }

    Some(Ok(BorsCommand::ChangeQueueState(QueueProperties {
        approval: Some(Approval { approver, mode }),
        priority,
        rollup,
        note,
    })))
}

/// Parses "rollup=<never/iffy/maybe/always>"
pub(super) fn parser_rollup(command: &CommandPart<'_>, parts: &[CommandPart<'_>]) -> ParseResult {
    let rollup_mode = match parse_rollup(std::slice::from_ref(command))? {
        Ok(r) => r,
        Err(e) => return Some(Err(e)),
    };
    let note = parse_note(parts);
    Some(Ok(BorsCommand::ChangeQueueState(QueueProperties {
        approval: None,
        rollup: Some(rollup_mode),
        priority: None,
        note,
    })))
}

/// Parses the first occurrence of `rollup=<never/iffy/maybe/always>` in `parts`.
fn parse_rollup(parts: &[CommandPart<'_>]) -> ParseResult<RollupMode> {
    parts
        .iter()
        .filter_map(|part| match part {
            CommandPart::Bare("rollup-") => Some(Ok(RollupMode::Maybe)),
            CommandPart::Bare("rollup") => Some(Ok(RollupMode::Always)),
            CommandPart::KeyValue {
                key: "rollup",
                value,
            } => match RollupMode::from_str(value) {
                Ok(mode) => Some(Ok(mode)),
                Err(error) => Some(Err(CommandParseError::ValidationError(error))),
            },
            _ => None,
        })
        .next()
}

/// Parses "@bors p=<priority>"
pub(super) fn parser_priority(command: &CommandPart<'_>, parts: &[CommandPart<'_>]) -> ParseResult {
    let priority = match parse_priority(std::slice::from_ref(command))? {
        Ok(p) => p,
        Err(e) => return Some(Err(e)),
    };
    let note = parse_note(parts);
    Some(Ok(BorsCommand::ChangeQueueState(QueueProperties {
        approval: None,
        priority: Some(priority),
        rollup: None,
        note,
    })))
}

/// Parses the first occurrence of `p|priority=<priority>` in `parts`.
fn parse_priority(parts: &[CommandPart<'_>]) -> ParseResult<Priority> {
    parts
        .iter()
        .filter_map(|part| match part {
            CommandPart::KeyValue {
                key: "p" | "priority",
                value,
            } => Some(parse_priority_value(value)),
            _ => None,
        })
        .next()
}

pub(super) fn parse_priority_value(value: &str) -> Result<Priority, CommandParseError> {
    match value.parse::<Priority>() {
        Ok(p) => Ok(p),
        Err(_) => Err(CommandParseError::ValidationError(
            "Priority must be a non-negative integer".to_string(),
        )),
    }
}

fn parse_note(parts: &[CommandPart<'_>]) -> Option<String> {
    match parts {
        &[CommandPart::KeyValue { key: "note", value }] => Some(value.to_string()),
        parts => join_parts(parts),
    }
}
