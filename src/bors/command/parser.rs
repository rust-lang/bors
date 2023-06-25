//! Defines parsers for bors commands.

use std::collections::HashSet;

use crate::bors::command::BorsCommand;
use crate::github::CommitSha;

#[derive(Debug, PartialEq)]
pub enum CommandParseError<'a> {
    MissingCommand,
    UnknownCommand(&'a str),
    MissingArgValue { arg: &'a str },
    UnknownArg(&'a str),
    DuplicateArg(&'a str),
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
        // The order of the parsers in the vector is important
        let parsers: Vec<for<'b> fn(&'b str, &[CommandPart<'b>]) -> ParseResult<'b>> =
            vec![parser_ping, parser_try_cancel, parser_try];

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
                                    CommandPart::KeyValue { .. } => {
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

/// Parses "@bors ping".
fn parser_ping<'a>(command: &'a str, _parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "ping" {
        Some(Ok(BorsCommand::Ping))
    } else {
        None
    }
}

/// Parses "@bors try <parent=sha>".
fn parser_try<'a>(command: &'a str, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command != "try" {
        return None;
    }

    let mut parent = None;

    for part in parts {
        match part {
            CommandPart::Bare(key) => {
                return Some(Err(CommandParseError::UnknownArg(key)));
            }
            CommandPart::KeyValue { key, value } => {
                if *key == "parent" {
                    parent = Some(value);
                } else {
                    return Some(Err(CommandParseError::UnknownArg(key)));
                }
            }
        }
    }
    Some(Ok(BorsCommand::Try {
        parent: parent.map(|v| CommitSha(v.to_string())),
    }))
}

/// Parses "@bors try cancel".
fn parser_try_cancel<'a>(command: &'a str, parts: &[CommandPart<'a>]) -> ParseResult<'a> {
    if command == "try" && parts.get(0) == Some(&CommandPart::Bare("cancel")) {
        Some(Ok(BorsCommand::TryCancel))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::{CommandParseError, CommandParser};
    use crate::bors::command::BorsCommand;
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
        assert!(matches!(cmds[0], Ok(BorsCommand::Try { parent: None })));
    }

    #[test]
    fn parse_try() {
        let cmds = parse_commands("@bors try");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Try { parent: None })));
    }

    #[test]
    fn parse_try_parent() {
        let cmds = parse_commands("@bors try parent=foo");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: Some(CommitSha("foo".to_string()))
            })
        );
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
        assert!(matches!(cmds[0], Ok(BorsCommand::Try { parent: None })));
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
