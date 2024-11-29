//! Defines parsers for bors commands.

use std::collections::HashSet;

use crate::bors::command::{Approver, BorsCommand, Parent};
use crate::github::CommitSha;

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

#[derive(Clone)]
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
        // The order of the parsers in the vector is important
        let parsers: Vec<for<'b> fn(&'b str, &[CommandPart<'b>]) -> ParseResult<'b>> = vec![
            parse_self_approve,
            parse_unapprove,
            parser_help,
            parser_ping,
            parser_try_cancel,
            parser_try,
        ];

        text.lines()
            .filter_map(|line| match line.find(&self.prefix) {
                Some(index) => {
                    let command = &line[index + self.prefix.len()..];
                    match parse_parts(command) {
                        Ok(parts) => {
                            if parts.is_empty() {
                                Some(Err(CommandParseError::MissingCommand))
                            } else {
                                let (command, rest) = parts.split_at(1);
                                match command[0] {
                                    CommandPart::Bare(command) => {
                                        for parser in &parsers {
                                            if let Some(result) = parser(command, rest) {
                                                return Some(result);
                                            }
                                        }
                                        Some(Err(CommandParseError::UnknownCommand(command)))
                                    }
                                    // for `@bors r=user1`
                                    CommandPart::KeyValue { .. } => {
                                        if let Some(result) = parse_approve_on_behalf(&parts) {
                                            return Some(result);
                                        }
                                        Some(Err(CommandParseError::MissingCommand))
                                    }
                                }
                            }
                        }
                        Err(error) => Some(Err(error)),
                    }
                }
                None => None,
            })
            .collect()
    }
}

type ParseResult<'a> = Option<Result<BorsCommand, CommandParseError<'a>>>;

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

/// Parsers

/// Parses "@bors r+"
fn parse_self_approve<'a>(command: &'a str, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "r+" {
        Some(Ok(BorsCommand::Approve(Approver::Myself)))
    } else {
        None
    }
}

/// Parses "@bors r=<username>".
fn parse_approve_on_behalf<'a>(parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if let Some(CommandPart::KeyValue { key, value }) = parts.first() {
        if *key != "r" {
            Some(Err(CommandParseError::UnknownArg(key)))
        } else if value.is_empty() {
            return Some(Err(CommandParseError::MissingArgValue { arg: "r" }));
        } else {
            Some(Ok(BorsCommand::Approve(Approver::Specified(
                value.to_string(),
            ))))
        }
    } else {
        Some(Err(CommandParseError::MissingArgValue { arg: "r" }))
    }
}

/// Parses "@bors r-"
fn parse_unapprove<'a>(command: &'a str, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "r-" {
        Some(Ok(BorsCommand::Unapprove))
    } else {
        None
    }
}

/// Parses "@bors help".
fn parser_help<'a>(command: &'a str, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "help" {
        Some(Ok(BorsCommand::Help))
    } else {
        None
    }
}

/// Parses "@bors ping".
fn parser_ping<'a>(command: &'a str, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "ping" {
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
fn parser_try<'a>(command: &'a str, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command != "try" {
        return None;
    }

    let mut parent = None;
    let mut jobs = Vec::new();

    for part in parts {
        match part {
            CommandPart::Bare(key) => {
                return Some(Err(CommandParseError::UnknownArg(key)));
            }
            CommandPart::KeyValue { key, value } => match *key {
                "parent" => {
                    parent = if *value == "last" {
                        Some(Parent::Last)
                    } else {
                        match parse_sha(value) {
                            Ok(sha) => Some(Parent::CommitSha(sha)),
                            Err(error) => {
                                return Some(Err(CommandParseError::ValidationError(format!(
                                    "Try parent has to be a valid commit SHA: {error}"
                                ))));
                            }
                        }
                    }
                }
                "jobs" => {
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
fn parser_try_cancel<'a>(command: &'a str, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "try" && parts.first() == Some(&CommandPart::Bare("cancel")) {
        Some(Ok(BorsCommand::TryCancel))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::{CommandParseError, CommandParser};
    use crate::bors::command::{Approver, BorsCommand, Parent};
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
        assert!(matches!(
            cmds[0],
            Ok(BorsCommand::Approve(Approver::Myself))
        ));
    }

    #[test]
    fn parse_approve_on_behalf() {
        let cmds = parse_commands("@bors r=user1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Ok(
            Approve(
                Specified(
                    "user1",
                ),
            ),
        )
        "###);
    }

    #[test]
    fn parse_approve_on_behalf_of_only_one_approver() {
        let cmds = parse_commands("@bors r=user1,user2");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r###"
        Ok(
            Approve(
                Specified(
                    "user1,user2",
                ),
            ),
        )
        "###);
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
    fn parse_try_cancel() {
        let cmds = parse_commands("@bors try cancel");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::TryCancel)));
    }

    fn parse_commands(text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
        CommandParser::new("@bors".to_string()).parse_commands(text)
    }
}
