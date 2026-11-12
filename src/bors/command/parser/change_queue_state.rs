use crate::bors::RollupMode;
use crate::bors::command::parser::CommandPart::{Bare, KeyValue};
use crate::bors::command::parser::{CommandPart, ParseResult, join_parts};
use crate::bors::command::{
    Approval, Approver, BorsCommand, CommandParseError, Priority, QueueProperties,
    SquashCommitMessage,
};
use crate::database::ApprovalMode;
use std::str::FromStr;

const SIGNATURE: &str = "[r+] [r=<user>] [p=<priority>] [rollup=<never|iffy|maybe|always>] [force] [note=<note>] [squash [message/msg=<msg>]]";

#[allow(clippy::collapsible_if)]
pub(super) fn parser_change_queue_state(
    command: &CommandPart<'_>,
    mut parts: &[CommandPart<'_>],
) -> ParseResult {
    let mut result = QueueProperties {
        approval: None,
        priority: None,
        rollup: None,
        note: None,
    };
    let mut squash = None;
    if let Err(e) = parse_part(command, &mut result, &mut squash)? {
        return Some(Err(e));
    };
    loop {
        match parts {
            // Next argument
            [part, rest @ ..] if let Some(res) = parse_part(part, &mut result, &mut squash) => {
                if let Err(e) = res {
                    return Some(Err(e));
                }
                parts = rest;
            }
            // If the first argument is `KeyValue`, we think this is more likely a mistyped argument than a note
            [KeyValue { key, .. }, ..] => {
                return Some(Err(CommandParseError::UnknownArg {
                    arg: key.to_string(),
                    did_you_mean: SIGNATURE.to_string(),
                }));
            }
            // If `result` is still empty, we also think this is most likely a mistyped argument
            [part, ..] if result.is_empty() => {
                return Some(Err(CommandParseError::UnknownArg {
                    arg: part.as_key().to_string(),
                    did_you_mean: SIGNATURE.to_string(),
                }));
            }
            // The rest is a note
            rest @ [Bare(_), ..] => {
                if let Some(note) = join_parts(rest) {
                    if let Err(e) = insert_check_dup("note", &mut result.note, note)? {
                        return Some(Err(e));
                    }
                }
                break;
            }
            [] => break,
        }
    }

    Some(Ok(if let Some(commit_message) = squash {
        if result.is_empty() {
            BorsCommand::Squash { commit_message }
        } else {
            BorsCommand::SquashApprove {
                commit_message,
                approval_info: result,
            }
        }
    } else {
        BorsCommand::ChangeQueueState(result)
    }))
}

type PartFunction =
    fn(&CommandPart<'_>, &mut QueueProperties, &mut Option<SquashCommitMessage>) -> ParseResult<()>;
const PART_PARSERS: &[PartFunction] = &[
    parse_part_approval,
    parse_part_priority,
    parse_part_rollup,
    parse_part_note,
    parse_part_force,
    parse_part_squash,
    parse_part_squash_message,
];

fn parse_part(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    for parser in PART_PARSERS {
        if let Some(result) = parser(part, result, squash) {
            return Some(result);
        }
    }
    None
}

fn parse_part_approval(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    _squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    let approver = match part {
        Bare("r+") => Approver::Myself,
        KeyValue { key: "r", value } => {
            if value.is_empty() {
                return Some(Err(CommandParseError::MissingArgValue {
                    arg: "r".to_string(),
                }));
            }
            Approver::Specified(value.to_string())
        }
        _ => return None,
    };
    insert_check_dup(
        "approval",
        &mut result.approval,
        Approval {
            approver,
            mode: ApprovalMode::Tentative,
        },
    )
}

fn parse_part_force(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    _squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    match part {
        Bare("force") => {}
        _ => return None,
    }
    let Some(approval) = &mut result.approval else {
        return Some(Err(CommandParseError::ValidationError(
            "The `force` argument cannot be used without an approval".to_string(),
        )));
    };
    if approval.mode != ApprovalMode::Tentative {
        return Some(Err(CommandParseError::DuplicateArg("force".to_string())));
    }
    approval.mode = ApprovalMode::Eager;
    Some(Ok(()))
}

fn parse_part_rollup(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    _squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    let rollup = match part {
        Bare("rollup-") => RollupMode::Maybe,
        Bare("rollup") => RollupMode::Always,
        KeyValue {
            key: "rollup",
            value,
        } => match RollupMode::from_str(value) {
            Ok(mode) => mode,
            Err(error) => return Some(Err(CommandParseError::ValidationError(error))),
        },
        _ => return None,
    };
    insert_check_dup("rollup", &mut result.rollup, rollup)
}

fn parse_part_priority(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    _squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    let priority = match part {
        KeyValue {
            key: "p" | "priority",
            value,
        } => match parse_priority_value(value) {
            Ok(prio) => prio,
            Err(err) => return Some(Err(err)),
        },
        _ => return None,
    };
    insert_check_dup("priority", &mut result.priority, priority)
}

fn parse_part_note(
    part: &CommandPart<'_>,
    result: &mut QueueProperties,
    _squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    let note = match part {
        KeyValue { key: "note", value } => value.to_string(),
        _ => return None,
    };
    insert_check_dup("note", &mut result.note, note)
}

fn parse_part_squash(
    part: &CommandPart<'_>,
    _result: &mut QueueProperties,
    squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    match part {
        Bare("squash") => {}
        _ => return None,
    };
    insert_check_dup("squash", squash, SquashCommitMessage::AutoGenerate)
}

fn parse_part_squash_message(
    part: &CommandPart<'_>,
    _result: &mut QueueProperties,
    squash: &mut Option<SquashCommitMessage>,
) -> ParseResult<()> {
    let msg = match part {
        KeyValue {
            key: "msg" | "message",
            value,
        } => {
            if *value == "description" {
                SquashCommitMessage::PullRequestDescription
            } else {
                SquashCommitMessage::Explicit(value.to_string())
            }
        }
        _ => return None,
    };
    let Some(squash) = squash else {
        return Some(Err(CommandParseError::ValidationError(
            "The `msg` argument cannot be used without `squash`".to_string(),
        )));
    };
    if *squash != SquashCommitMessage::AutoGenerate {
        return Some(Err(CommandParseError::DuplicateArg("force".to_string())));
    }
    *squash = msg;
    Some(Ok(()))
}

fn insert_check_dup<T>(arg_name: &str, opt: &mut Option<T>, v: T) -> ParseResult<()> {
    match opt {
        None => {
            *opt = Some(v);
            Some(Ok(()))
        }
        Some(_) => Some(Err(CommandParseError::DuplicateArg(arg_name.to_string()))),
    }
}

pub(super) fn parse_priority_value(value: &str) -> Result<Priority, CommandParseError> {
    match value.parse::<Priority>() {
        Ok(p) => Ok(p),
        Err(_) => Err(CommandParseError::ValidationError(
            "Priority must be a non-negative integer".to_string(),
        )),
    }
}
