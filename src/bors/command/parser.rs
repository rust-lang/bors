//! Defines parsers for bors commands.

use std::collections::HashSet;
use std::str::FromStr;

use crate::bors::command::{Approver, BorsCommand, Parent};
use crate::github::CommitSha;

use super::{Priority, RollupMode};

#[derive(Debug, PartialEq)]
pub enum CommandParseError<'a> {
    MissingCommand,
    UnknownCommand(&'a str),
    MissingArgValue { arg: &'a str },
    UnknownArg(&'a str),
    DuplicateArg(&'a str),
    ValidationError(String),
}

/// Part of a command, either a bare string like `try` or a key value like `parent=<sha>`.
#[derive(PartialEq)]
enum CommandPart<'a> {
    Bare(&'a str),
    KeyValue { key: &'a str, value: &'a str },
}

pub struct CommandParser {
    prefix: String,
}

impl CommandParser {
    pub fn new(prefix: String) -> Self {
        Self { prefix }
    }

    /// Parses bors commands from the given string.
    ///
    /// Assumes that each command spands at most one line and that there are not more commands on
    /// each line.
    pub fn parse_commands<'a>(
        &self,
        text: &'a str,
    ) -> Vec<Result<BorsCommand, CommandParseError<'a>>> {
        text.lines()
            .filter_map(|line| match line.find(&self.prefix) {
                Some(index) => {
                    let input = &line[index + self.prefix.len()..];
                    parse_command(input)
                }
                None => None,
            })
            .collect()
    }
}

type ParseResult<'a, T = BorsCommand> = Option<Result<T, CommandParseError<'a>>>;

// The order of the parsers in the vector is important
const PARSERS: &[for<'b> fn(&CommandPart<'b>, &[CommandPart<'b>]) -> ParseResult<'b>] = &[
    parser_approval,
    parser_unapprove,
    parser_rollup,
    parser_priority,
    parser_try_cancel,
    parser_try,
    parser_delegation,
    parser_info,
    parser_help,
    parser_ping,
];

fn parse_command(input: &str) -> ParseResult {
    match parse_parts(input) {
        Ok(parts) => match parts.as_slice() {
            [] => Some(Err(CommandParseError::MissingCommand)),
            [command, arguments @ ..] => {
                for parser in PARSERS {
                    if let Some(result) = parser(command, arguments) {
                        return Some(result);
                    }
                }
                let unknown = match command {
                    CommandPart::Bare(c) => c,
                    CommandPart::KeyValue { key, .. } => key,
                };
                Some(Err(CommandParseError::UnknownCommand(unknown)))
            }
        },
        Err(error) => Some(Err(error)),
    }
}

fn parse_parts(input: &str) -> Result<Vec<CommandPart>, CommandParseError> {
    let mut parts = vec![];
    let mut seen_keys = HashSet::new();

    for item in input.split_whitespace() {
        // Stop parsing, as this is a command for another bot, such as `@rust-timer queue`.
        if item.starts_with('@') {
            break;
        }

        match item.split_once('=') {
            Some((key, value)) => {
                if value.is_empty() {
                    return Err(CommandParseError::MissingArgValue { arg: key });
                }
                if seen_keys.contains(key) {
                    return Err(CommandParseError::DuplicateArg(key));
                }
                seen_keys.insert(key);
                parts.push(CommandPart::KeyValue { key, value });
            }
            None => parts.push(CommandPart::Bare(item)),
        }
    }
    Ok(parts)
}

/// Parses:
/// - "@bors r+ [p=<priority>] [rollup=<never|iffy|maybe|always>]"
/// - "@bors r=<user> [p=<priority>] [rollup=<never|iffy|maybe|always>]"
fn parser_approval<'a>(command: &CommandPart<'a>, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    let approver = match command {
        CommandPart::Bare("r+") => Approver::Myself,
        CommandPart::KeyValue { key: "r", value } => {
            if value.is_empty() {
                return Some(Err(CommandParseError::MissingArgValue { arg: "r" }));
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
    Some(Ok(BorsCommand::Approve {
        approver,
        priority,
        rollup,
    }))
}

/// Parses "@bors r-"
fn parser_unapprove<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if let CommandPart::Bare("r-") = command {
        Some(Ok(BorsCommand::Unapprove))
    } else {
        None
    }
}

/// Parses "@bors help".
fn parser_help<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if let CommandPart::Bare("help") = command {
        Some(Ok(BorsCommand::Help))
    } else {
        None
    }
}

/// Parses "@bors ping".
fn parser_ping<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if let CommandPart::Bare("ping") = command {
        Some(Ok(BorsCommand::Ping))
    } else {
        None
    }
}

fn parse_sha(input: &str) -> Result<CommitSha, String> {
    if input.len() != 40 {
        return Err("SHA must have exactly 40 characters".to_string());
    }
    Ok(CommitSha(input.to_string()))
}

/// Parses "@bors try <parent=sha>".
fn parser_try<'a>(command: &CommandPart<'a>, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if *command != CommandPart::Bare("try") {
        return None;
    }

    let mut parent = None;
    let mut jobs = Vec::new();

    for part in parts {
        match part {
            CommandPart::Bare(key) => {
                return Some(Err(CommandParseError::UnknownArg(key)));
            }
            CommandPart::KeyValue { key, value } => match (*key, *value) {
                ("parent", "last") => parent = Some(Parent::Last),
                ("parent", value) => match parse_sha(value) {
                    Ok(sha) => parent = Some(Parent::CommitSha(sha)),
                    Err(error) => {
                        return Some(Err(CommandParseError::ValidationError(format!(
                            "Try parent has to be a valid commit SHA: {error}"
                        ))));
                    }
                },
                ("jobs", value) => {
                    let raw_jobs: Vec<_> = value.split(',').map(|s| s.to_string()).collect();
                    if raw_jobs.is_empty() {
                        return Some(Err(CommandParseError::ValidationError(
                            "Try jobs must not be empty".to_string(),
                        )));
                    }

                    // rust ci currently allows specifying 10 jobs max
                    if raw_jobs.len() > 10 {
                        return Some(Err(CommandParseError::ValidationError(
                            "Try jobs must not have more than 10 jobs".to_string(),
                        )));
                    }
                    jobs = raw_jobs;
                }
                _ => {
                    return Some(Err(CommandParseError::UnknownArg(key)));
                }
            },
        }
    }
    Some(Ok(BorsCommand::Try { parent, jobs }))
}

/// Parses "@bors try cancel".
fn parser_try_cancel<'a>(command: &CommandPart<'a>, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    match (command, parts) {
        (CommandPart::Bare("try"), [CommandPart::Bare("cancel"), ..]) => {
            Some(Ok(BorsCommand::TryCancel))
        }
        _ => None,
    }
}

/// Parses "@bors delegate+" and "@bors delegate-".
fn parser_delegation<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    match command {
        CommandPart::Bare("delegate+") => Some(Ok(BorsCommand::Delegate)),
        CommandPart::Bare("delegate-") => Some(Ok(BorsCommand::Undelegate)),
        _ => None,
    }
}

fn parse_priority_value(value: &str) -> Result<Priority, CommandParseError> {
    match value.parse::<Priority>() {
        Ok(p) => Ok(p),
        Err(_) => Err(CommandParseError::ValidationError(
            "Priority must be a non-negative integer".to_string(),
        )),
    }
}

/// Parses the first occurrence of `p|priority=<priority>` in `parts`.
fn parse_priority<'a>(parts: &[CommandPart<'a>]) -> ParseResult<'a, Priority> {
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

/// Parses "@bors p=<priority>"
fn parser_priority<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    parse_priority(std::slice::from_ref(command)).map(|res| res.map(BorsCommand::SetPriority))
}

/// Parses the first occurrence of `rollup=<never/iffy/maybe/always>` in `parts`.
fn parse_rollup<'a>(parts: &[CommandPart<'a>]) -> ParseResult<'a, RollupMode> {
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

/// Parses "rollup=<never/iffy/maybe/always>"
fn parser_rollup<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    parse_rollup(std::slice::from_ref(command)).map(|res| res.map(BorsCommand::SetRollupMode))
}

/// Parses "@bors info"
fn parser_info<'a>(command: &CommandPart<'a>, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if *command == CommandPart::Bare("info") {
        Some(Ok(BorsCommand::Info))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::{CommandParseError, CommandParser};
    use crate::bors::command::{Approver, BorsCommand, Parent, RollupMode};
    use crate::github::CommitSha;

    #[test]
    fn no_commands() {
        let cmds = parse_commands(r#"Hi, this PR looks nice!"#);
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn missing_command() {
        let cmds = parse_commands("@bors");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::MissingCommand)));
    }

    #[test]
    fn unknown_command() {
        let cmds = parse_commands("@bors foo");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Err(CommandParseError::UnknownCommand("foo"))
        ));
    }

    #[test]
    fn parse_arg_no_value() {
        let cmds = parse_commands("@bors ping a=");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Err(CommandParseError::MissingArgValue { arg: "a" })
        ));
    }

    #[test]
    fn parse_duplicate_key() {
        let cmds = parse_commands("@bors ping a=b a=c");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::DuplicateArg("a"))));
    }

    #[test]
    fn parse_default_approve() {
        let cmds = parse_commands("@bors r+");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: None,
                rollup: None,
            })
        );
    }

    #[test]
    fn parse_approve_on_behalf() {
        let cmds = parse_commands("@bors r=user1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Ok(
            Approve {
                approver: Specified(
                    "user1",
                ),
                priority: None,
                rollup: None,
            },
        )
        "#);
    }

    #[test]
    fn parse_approve_on_behalf_of_only_one_approver() {
        let cmds = parse_commands("@bors r=user1,user2");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Ok(
            Approve {
                approver: Specified(
                    "user1,user2",
                ),
                priority: None,
                rollup: None,
            },
        )
        "#);
    }

    #[test]
    fn parse_approve_with_priority() {
        let cmds = parse_commands("@bors r+ p=1");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: Some(1),
                rollup: None
            })
        )
    }

    #[test]
    fn parse_approve_on_behalf_with_priority() {
        let cmds = parse_commands("@bors r=user1 p=2");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user1".to_string()),
                priority: Some(2),
                rollup: None
            })
        )
    }

    #[test]
    fn parse_multiple_priority_commands() {
        let cmds = parse_commands(
            r#"
            @bors r+ p=1
            @bors r=user2 p=2
        "#,
        );
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: Some(1),
                rollup: None
            })
        );
        assert_eq!(
            cmds[1],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user2".to_string()),
                priority: Some(2),
                rollup: None
            })
        );
    }

    #[test]
    fn parse_approve_negative_priority_invalid() {
        let cmds = parse_commands("@bors r+ p=-1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "###);
    }

    #[test]
    fn parse_approve_on_behalf_with_priority_alias() {
        let cmds = parse_commands("@bors r=user1 priority=2");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user1".to_string()),
                priority: Some(2),
                rollup: None
            })
        )
    }

    #[test]
    fn parse_approve_priority_invalid() {
        let cmds = parse_commands("@bors r+ p=abc");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "###);
    }

    #[test]
    fn parse_approve_priority_empty() {
        let cmds = parse_commands("@bors r+ p=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            MissingArgValue {
                arg: "p",
            },
        )
        "###);
    }

    #[test]
    fn parse_priority_exceeds_max_u32() {
        let cmds = parse_commands("@bors p=4294967296");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "###);
    }

    #[test]
    fn parse_priority() {
        let cmds = parse_commands("@bors p=5");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetPriority(5)));
    }

    #[test]
    fn parse_priority_alias() {
        let cmds = parse_commands("@bors priority=5");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetPriority(5)));
    }

    #[test]
    fn parse_priority_empty() {
        let cmds = parse_commands("@bors p=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            MissingArgValue {
                arg: "p",
            },
        )
        "###);
    }

    #[test]
    fn parse_priority_invalid() {
        let cmds = parse_commands("@bors p=abc");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "###);
    }

    #[test]
    fn parse_priority_negative() {
        let cmds = parse_commands("@bors p=-1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "###);
    }

    #[test]
    fn parse_duplicate_priority() {
        let cmds = parse_commands("@bors p=1 p=2");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::DuplicateArg("p"))));
    }

    #[test]
    fn parse_priority_unknown_arg() {
        let cmds = parse_commands("@bors p=1 a");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetPriority(1)));
    }

    #[test]
    fn parse_approve_with_rollup() {
        let cmds = parse_commands("@bors r+ rollup");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: None,
                rollup: Some(RollupMode::Always)
            })
        )
    }

    #[test]
    fn parse_approve_on_behalf_with_rollup_value() {
        let cmds = parse_commands("@bors r=user1 rollup=never");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user1".to_string()),
                priority: None,
                rollup: Some(RollupMode::Never)
            })
        )
    }

    #[test]
    fn parse_approve_on_behalf_with_rollup_bare() {
        let cmds = parse_commands("@bors r=user1 rollup");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user1".to_string()),
                priority: None,
                rollup: Some(RollupMode::Always)
            })
        )
    }

    #[test]
    fn parse_approve_on_behalf_with_rollup_bare_maybe() {
        let cmds = parse_commands("@bors r=user1 rollup-");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user1".to_string()),
                priority: None,
                rollup: Some(RollupMode::Maybe)
            })
        )
    }

    #[test]
    fn parse_multiple_rollup_commands() {
        let cmds = parse_commands(
            r#"
            @bors r+ rollup
            @bors r=user2 rollup=iffy
        "#,
        );
        assert_eq!(cmds.len(), 2);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: None,
                rollup: Some(RollupMode::Always)
            })
        );
        assert_eq!(
            cmds[1],
            Ok(BorsCommand::Approve {
                approver: Approver::Specified("user2".to_string()),
                priority: None,
                rollup: Some(RollupMode::Iffy)
            })
        );
    }

    #[test]
    fn parse_approve_with_invalid_rollup_status() {
        let cmds = parse_commands("@bors r+ rollup=abc");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Invalid rollup mode `abc`. Possible values are always/iffy/never/maybe",
            ),
        )
        "#);
    }

    #[test]
    fn parse_approve_rollup_empty() {
        let cmds = parse_commands("@bors r+ rollup=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            MissingArgValue {
                arg: "rollup",
            },
        )
        "###);
    }

    #[test]
    fn parse_rollup_bare() {
        let cmds = parse_commands("@bors rollup");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetRollupMode(RollupMode::Always)));
    }

    #[test]
    fn parse_rollup_bare_maybe() {
        let cmds = parse_commands("@bors rollup-");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetRollupMode(RollupMode::Maybe)));
    }

    #[test]
    fn parse_priority_rollup() {
        let cmds = parse_commands("@bors rollup=always");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetRollupMode(RollupMode::Always)));
    }

    #[test]
    fn parse_rollup_empty() {
        let cmds = parse_commands("@bors rollup=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            MissingArgValue {
                arg: "rollup",
            },
        )
        "###);
    }

    #[test]
    fn parse_rollup_invalid_with_int() {
        let cmds = parse_commands("@bors rollup=3");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Invalid rollup mode `3`. Possible values are always/iffy/never/maybe",
            ),
        )
        "#);
    }

    #[test]
    fn parse_rollup_unknown_arg() {
        let cmds = parse_commands("@bors rollup a");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::SetRollupMode(RollupMode::Always)));
    }

    #[test]
    fn parse_approve_with_rollup_bare_priority() {
        let cmds = parse_commands("@bors r+ rollup p=1");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: Some(1),
                rollup: Some(RollupMode::Always)
            })
        );
    }

    #[test]
    fn parse_approve_with_rollup_value_priority() {
        let cmds = parse_commands("@bors r+ rollup=iffy p=1");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Approve {
                approver: Approver::Myself,
                priority: Some(1),
                rollup: Some(RollupMode::Iffy)
            })
        );
    }

    #[test]
    fn parse_unapprove() {
        let cmds = parse_commands("@bors r-");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Unapprove)));
    }

    #[test]
    fn parse_help() {
        let cmds = parse_commands("@bors help");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Help)));
    }

    #[test]
    fn parse_help_unknown_arg() {
        let cmds = parse_commands("@bors help a");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Help)));
    }

    #[test]
    fn parse_ping() {
        let cmds = parse_commands("@bors ping");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Ping)));
    }

    #[test]
    fn parse_ping_unknown_arg() {
        let cmds = parse_commands("@bors ping a");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Ping)));
    }

    #[test]
    fn parse_command_multiline() {
        let cmds = parse_commands(
            r#"
line one
@bors try
line two
"#,
        );
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        "###);
    }

    #[test]
    fn parse_try() {
        let cmds = parse_commands("@bors try");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        "###);
    }

    #[test]
    fn parse_try_parent() {
        let cmds = parse_commands("@bors try parent=ea9c1b050cc8b420c2c211d2177811e564a4dc60");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: Some(Parent::CommitSha(CommitSha(
                    "ea9c1b050cc8b420c2c211d2177811e564a4dc60".to_string()
                ))),
                jobs: Vec::new()
            })
        );
    }

    #[test]
    fn parse_try_parent_last() {
        let cmds = parse_commands("@bors try parent=last");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: Some(Parent::Last),
                jobs: Vec::new()
            })
        );
    }

    #[test]
    fn parse_try_parent_invalid() {
        let cmds = parse_commands("@bors try parent=foo");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Try parent has to be a valid commit SHA: SHA must have exactly 40 characters",
            ),
        )
        "###);
    }

    #[test]
    fn parse_try_jobs() {
        let cmds = parse_commands("@bors try jobs=ci,lint");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: None,
                jobs: vec!["ci".to_string(), "lint".to_string()]
            })
        );
    }

    #[test]
    fn parse_try_jobs_empty() {
        let cmds = parse_commands("@bors try jobs=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            MissingArgValue {
                arg: "jobs",
            },
        )
        "###);
    }

    #[test]
    fn parse_try_jobs_too_many() {
        let cmds =
            parse_commands("@bors try jobs=ci,lint,foo,bar,baz,qux,quux,corge,grault,garply,waldo");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Err(
            ValidationError(
                "Try jobs must not have more than 10 jobs",
            ),
        )
        "###);
    }

    #[test]
    fn parse_try_unknown_arg() {
        let cmds = parse_commands("@bors try a");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::UnknownArg("a"))));
    }

    #[test]
    fn parse_try_unknown_kv_arg() {
        let cmds = parse_commands("@bors try a=b");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::UnknownArg("a"))));
    }

    #[test]
    fn parse_try_with_rust_timer() {
        let cmds = parse_commands(
            r#"
@bors try @rust-timer queue
"#,
        );
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        "###)
    }

    #[test]
    fn parse_info() {
        let cmds = parse_commands("@bors info");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Info)));
    }

    #[test]
    fn parse_info_unknown_arg() {
        let cmds = parse_commands("@bors info a");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Info)));
    }

    #[test]
    fn parse_try_cancel() {
        let cmds = parse_commands("@bors try cancel");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::TryCancel)));
    }

    #[test]
    fn parse_delegate_author() {
        let cmds = parse_commands("@bors delegate+");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Delegate)));
    }

    #[test]
    fn parse_undelegate() {
        let cmds = parse_commands("@bors delegate-");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::Undelegate));
    }

    #[test]
    fn parse_delegate_author_unknown_arg() {
        let cmds = parse_commands("@bors delegate+ a");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::Delegate));
    }

    fn parse_commands(text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
        CommandParser::new("@bors".to_string()).parse_commands(text)
    }
}
