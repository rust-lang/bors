//! Defines parsers for bors commands.

use crate::bors::command::BorsCommand;
use std::collections::HashMap;
use std::iter::Peekable;
use std::str::SplitWhitespace;

#[derive(Debug)]
pub enum CommandParseError<'a> {
    MissingCommand,
    UnknownCommand(&'a str),
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
        let parsers: Vec<fn(Tokenizer) -> ParseResult> =
            vec![parser_ping, parser_try_cancel, parser_try];

        text.lines()
            .filter_map(|line| match line.find(&self.prefix) {
                Some(index) => {
                    let command = &line[index + self.prefix.len()..];
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

#[allow(unused)]
pub fn parse_key_value(input: &str) -> Result<HashMap<String, String>, String> {
    let mut kv_pairs = HashMap::new();

    for pair in input.split_whitespace() {
        if let Some((key, value)) = pair.split_once('=') {
            kv_pairs.insert(key.to_string(), value.to_string());
        }
    }

    Ok(kv_pairs)
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::parse_key_value;
    use crate::bors::command::parser::{CommandParseError, CommandParser};
    use crate::bors::command::BorsCommand;
    use std::collections::HashMap;

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
    fn test_parse_key_value_empty_input() {
        assert_eq!(parse_key_value(""), Ok(HashMap::new()));
    }

    #[test]
    fn test_parse_key_value_valid_input() {
        assert_eq!(
            parse_key_value("key1=value1 key2=value2"),
            Ok(vec![
                ("key1".to_string(), "value1".to_string()),
                ("key2".to_string(), "value2".to_string()),
            ]
            .into_iter()
            .collect())
        );
    }

    #[test]
    fn test_parse_key_value_duplicate_keys() {
        assert_eq!(
            parse_key_value("key=value1 key=value2"),
            Ok(vec![("key".to_string(), "value2".to_string())]
                .into_iter()
                .collect())
        );
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

    fn parse_commands(text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
        CommandParser::new("@bors".to_string()).parse_commands(text)
    }
}
