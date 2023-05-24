//! Defines parsers for bors commands.

use crate::bors::command::BorsCommand;
use std::iter::Peekable;
use std::str::SplitWhitespace;

#[derive(Debug)]
pub enum CommandParseError<'a> {
    MissingCommand,
    UnknownCommand(&'a str),
}

pub const BOT_PREFIX: &str = "@bors";

/// Parses bors commands from the given string.
///
/// Assumes that each command spands at most one line and that there are not more commands on
/// each line.
pub fn parse_commands(text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
    // The order of the parsers in the vector is important
    let parsers: Vec<fn(Tokenizer) -> ParseResult> =
        vec![parser_ping, parser_try_cancel, parser_try];

    text.lines()
        .filter_map(|line| match line.find(BOT_PREFIX) {
            Some(index) => {
                let command = &line[index + BOT_PREFIX.len()..];
                for parser in &parsers {
                    if let Some(result) = parser(Tokenizer::new(command)) {
                        return Some(result);
                    }
                }
                parser_wildcard(Tokenizer::new(command))
            }
            None => None,
        })
        .collect()
}

struct Tokenizer<'a> {
    iter: Peekable<SplitWhitespace<'a>>,
}

impl<'a> Tokenizer<'a> {
    fn new(input: &'a str) -> Self {
        Self {
            iter: input.split_whitespace().peekable(),
        }
    }

    fn peek(&mut self) -> Option<&'a str> {
        self.iter.peek().copied()
    }

    fn next(&mut self) -> Option<&'a str> {
        self.iter.next()
    }
}

type ParseResult<'a> = Option<Result<BorsCommand, CommandParseError<'a>>>;

/// Parsers

/// Parses "@bors ping".
fn parser_ping(tokenizer: Tokenizer) -> ParseResult {
    parse_exact("ping", BorsCommand::Ping, tokenizer)
}

/// Parses "@bors try".
fn parser_try(tokenizer: Tokenizer) -> ParseResult {
    parse_exact("try", BorsCommand::Try, tokenizer)
}

/// Parses "@bors try cancel".
fn parser_try_cancel(tokenizer: Tokenizer) -> ParseResult {
    parse_list(&["try", "cancel"], BorsCommand::TryCancel, tokenizer)
}

/// Returns either missing or unknown command error.
fn parser_wildcard(mut tokenizer: Tokenizer) -> ParseResult {
    let result = match tokenizer.peek() {
        Some(arg) => Err(CommandParseError::UnknownCommand(arg)),
        None => Err(CommandParseError::MissingCommand),
    };
    Some(result)
}

/// Checks if the tokenizer returns exactly `needle`.
/// If it does, the parser will return `result`.
fn parse_exact<'a>(
    needle: &'static str,
    result: BorsCommand,
    mut tokenizer: Tokenizer<'a>,
) -> ParseResult<'a> {
    match tokenizer.peek() {
        Some(word) if word == needle => Some(Ok(result)),
        _ => None,
    }
}

/// Checks if the tokenizer parses the following items in a sequence.
/// If it does, the parser will return `result`.
fn parse_list<'a>(
    needles: &[&'static str],
    result: BorsCommand,
    mut tokenizer: Tokenizer<'a>,
) -> ParseResult<'a> {
    for needle in needles {
        match tokenizer.peek() {
            Some(word) if word == *needle => {
                tokenizer.next();
            }
            _ => return None,
        }
    }
    Some(Ok(result))
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::{parse_commands, CommandParseError};
    use crate::bors::command::BorsCommand;

    #[test]
    fn test_no_commands() {
        let cmds = parse_commands(r#"Hi, this PR looks nice!"#);
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn test_missing_command() {
        let cmds = parse_commands("@bors");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Err(CommandParseError::MissingCommand)));
    }

    #[test]
    fn test_unknown_command() {
        let cmds = parse_commands("@bors foo");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Err(CommandParseError::UnknownCommand("foo"))
        ));
    }

    #[test]
    fn test_parse_ping() {
        let cmds = parse_commands("@bors ping");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Ping)));
    }

    #[test]
    fn test_parse_command_multiline() {
        let cmds = parse_commands(
            r#"
line one
@bors try
line two
"#,
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Try)));
    }

    #[test]
    fn test_parse_try() {
        let cmds = parse_commands("@bors try");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Try)));
    }

    #[test]
    fn test_parse_try_with_rust_timer() {
        let cmds = parse_commands(
            r#"
@bors try @rust-timer queue
"#,
        );
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Try)));
    }

    #[test]
    fn test_parse_try_cancel() {
        let cmds = parse_commands("@bors try cancel");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::TryCancel)));
    }
}
