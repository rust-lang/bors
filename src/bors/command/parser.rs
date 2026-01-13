//! Defines parsers for bors commands.

use crate::bors::command::{Approver, BorsCommand, CommandPrefix, Parent};
use crate::database::DelegatedPermission;
use crate::github::CommitSha;
use pulldown_cmark::{Event, Parser, Tag, TagEnd, TextMergeStream};
use std::collections::HashSet;
use std::str::FromStr;

use super::{Priority, RollupMode};

#[derive(Debug, PartialEq)]
pub enum CommandParseError {
    MissingCommand,
    UnknownCommand(String),
    MissingArgValue { arg: String },
    UnknownArg(String),
    DuplicateArg(String),
    ValidationError(String),
}

/// Part of a command, either a bare string like `try` or a key value like `parent=<sha>`.
#[derive(PartialEq, Copy, Clone)]
enum CommandPart<'a> {
    Bare(&'a str),
    KeyValue { key: &'a str, value: &'a str },
}

pub struct CommandParser {
    prefix: CommandPrefix,
    parsers: Vec<ParserFn>,
}

impl CommandParser {
    pub fn new(prefix: CommandPrefix) -> Self {
        Self {
            prefix,
            parsers: DEFAULT_PARSERS.to_vec(),
        }
    }

    /// Prefix of the bot, used to invoke commands from PR comments.
    /// For example `@bors`.
    pub fn prefix(&self) -> &CommandPrefix {
        &self.prefix
    }

    /// Parses bors commands from the given string.
    ///
    /// Assumes that each command spands at most one line and that there are not more commands on
    /// each line.
    pub fn parse_commands(&self, text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
        let segments = extract_text_from_markdown(text);
        segments
            .lines()
            .filter_map(|line| match line.find(self.prefix.as_ref()) {
                Some(index) => {
                    let input = &line[index + self.prefix.as_ref().len()..];
                    parse_command(input, &self.parsers)
                }
                None => None,
            })
            .collect()
    }
}

/// Extract text segments from a Markdown `text`.
fn extract_text_from_markdown(text: &str) -> String {
    let md_parser = TextMergeStream::new(Parser::new(text));
    let mut stack = vec![];
    let mut cleaned_text = String::new();

    for event in md_parser.into_iter() {
        match event {
            Event::Text(text) | Event::Html(text) => {
                // Only consider commands in raw text outside of wrapping elements
                if stack.is_empty() {
                    cleaned_text.push_str(&text);
                }
            }
            Event::SoftBreak | Event::HardBreak | Event::End(TagEnd::Paragraph) => {
                cleaned_text.push('\n');
            }
            Event::Start(tag) => match tag {
                // Ignore content in the following wrapping elements
                Tag::BlockQuote(_)
                | Tag::CodeBlock(_)
                | Tag::Link { .. }
                | Tag::Heading { .. }
                | Tag::Image { .. } => {
                    stack.push(tag);
                }
                // Special case to support `*foo*`, which is used for custom try jobs using `glob`.
                Tag::Emphasis => {
                    cleaned_text.push('*');
                }
                _ => {}
            },
            Event::End(TagEnd::Emphasis) => {
                cleaned_text.push('*');
            }
            Event::End(tag) => {
                if let Some(start_tag) = stack.last() {
                    match (start_tag, tag) {
                        (Tag::BlockQuote(_), TagEnd::BlockQuote(_))
                        | (Tag::CodeBlock(_), TagEnd::CodeBlock)
                        | (Tag::Link { .. }, TagEnd::Link)
                        | (Tag::Heading { .. }, TagEnd::Heading(_))
                        | (Tag::Image { .. }, TagEnd::Image) => {
                            stack.pop();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    cleaned_text
}

type ParseResult<T = BorsCommand> = Option<Result<T, CommandParseError>>;
type ParserFn = fn(&CommandPart<'_>, &[CommandPart<'_>]) -> ParseResult;

// The order of the parsers in the vector is important
const DEFAULT_PARSERS: &[ParserFn] = &[
    parser_approval,
    parser_unapprove,
    parser_rollup,
    parser_priority,
    parser_try_cancel,
    parser_try,
    parser_delegate,
    parser_undelegate,
    parser_info,
    parser_help,
    parser_ping,
    parser_retry,
    parser_tree_ops,
    parser_cancel,
];

fn parse_command(input: &str, parsers: &[ParserFn]) -> ParseResult {
    match parse_parts(input) {
        Ok(parts) => match parts.as_slice() {
            [] => Some(Err(CommandParseError::MissingCommand)),
            [command, arguments @ ..] => {
                for parser in parsers {
                    if let Some(result) = parser(command, arguments) {
                        return Some(result);
                    }
                }
                let unknown = match command {
                    CommandPart::Bare(c) => c,
                    CommandPart::KeyValue { key, .. } => key,
                };
                Some(Err(CommandParseError::UnknownCommand(unknown.to_string())))
            }
        },
        Err(error) => Some(Err(error)),
    }
}

fn parse_parts(input: &str) -> Result<Vec<CommandPart<'_>>, CommandParseError> {
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
                    return Err(CommandParseError::MissingArgValue {
                        arg: key.to_string(),
                    });
                }
                if seen_keys.contains(key) {
                    return Err(CommandParseError::DuplicateArg(key.to_string()));
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
fn parser_approval(command: &CommandPart<'_>, parts: &[CommandPart<'_>]) -> ParseResult {
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
    Some(Ok(BorsCommand::Approve {
        approver,
        priority,
        rollup,
    }))
}

/// Parses "@bors r-"
fn parser_unapprove(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    if let CommandPart::Bare("r-") = command {
        Some(Ok(BorsCommand::Unapprove))
    } else {
        None
    }
}

/// Parses "@bors help".
fn parser_help(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    if let CommandPart::Bare("help") = command {
        Some(Ok(BorsCommand::Help))
    } else {
        None
    }
}

/// Parses "@bors ping".
fn parser_ping(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
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
fn parser_try(command: &CommandPart<'_>, parts: &[CommandPart<'_>]) -> ParseResult {
    if *command != CommandPart::Bare("try") {
        return None;
    }

    let mut parent = None;
    let mut jobs = Vec::new();

    for part in parts {
        match part {
            CommandPart::Bare(key) => {
                return Some(Err(CommandParseError::UnknownArg(key.to_string())));
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
                ("job" | "jobs", value) => {
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
                    return Some(Err(CommandParseError::UnknownArg(key.to_string())));
                }
            },
        }
    }
    Some(Ok(BorsCommand::Try { parent, jobs }))
}

/// Parses "@bors try cancel".
fn parser_try_cancel(command: &CommandPart<'_>, parts: &[CommandPart<'_>]) -> ParseResult {
    match (command, parts) {
        (CommandPart::Bare("try"), [CommandPart::Bare("cancel"), ..]) => {
            Some(Ok(BorsCommand::TryCancel))
        }
        _ => None,
    }
}

/// Parses `@bors delegate=<try|review>` or `@bors delegate+`.
fn parser_delegate(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    match command {
        CommandPart::Bare("delegate+" | "delegate") => {
            Some(Ok(BorsCommand::SetDelegate(DelegatedPermission::Review)))
        }
        CommandPart::KeyValue {
            key: "delegate",
            value,
        } => match DelegatedPermission::from_str(value) {
            Ok(permission) => Some(Ok(BorsCommand::SetDelegate(permission))),
            Err(error) => Some(Err(CommandParseError::ValidationError(error))),
        },
        _ => None,
    }
}

/// Parses "@bors delegate-"
fn parser_undelegate(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    if let CommandPart::Bare("delegate-") = command {
        Some(Ok(BorsCommand::Undelegate))
    } else {
        None
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

/// Parses "@bors p=<priority>"
fn parser_priority(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    parse_priority(std::slice::from_ref(command)).map(|res| res.map(BorsCommand::SetPriority))
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

/// Parses "rollup=<never/iffy/maybe/always>"
fn parser_rollup(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    parse_rollup(std::slice::from_ref(command)).map(|res| res.map(BorsCommand::SetRollupMode))
}

/// Parses "@bors info"
fn parser_info(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    if *command == CommandPart::Bare("info") {
        Some(Ok(BorsCommand::Info))
    } else {
        None
    }
}

/// Parses `@bors retry`
fn parser_retry(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    if let CommandPart::Bare("retry") = command {
        Some(Ok(BorsCommand::Retry))
    } else {
        None
    }
}

/// Parses `@bors treeclosed-`, `@bors treeopen` and `@bors treeclosed=<priority>`
fn parser_tree_ops(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    match command {
        CommandPart::Bare("treeclosed-") | CommandPart::Bare("treeopen") => {
            Some(Ok(BorsCommand::OpenTree))
        }
        CommandPart::KeyValue {
            key: "treeclosed",
            value,
        } => {
            let priority = match parse_priority_value(value) {
                Ok(p) => p,
                Err(error) => return Some(Err(error)),
            };
            Some(Ok(BorsCommand::TreeClosed(priority)))
        }
        _ => None,
    }
}

/// Parses `@bors cancel` command.
/// Also has an alias of `@bors yield`.
fn parser_cancel(command: &CommandPart<'_>, _parts: &[CommandPart<'_>]) -> ParseResult {
    match command {
        CommandPart::Bare("cancel" | "yield") => Some(Ok(BorsCommand::Cancel)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::bors::command::parser::{CommandParseError, CommandParser};
    use crate::bors::command::{Approver, BorsCommand, Parent, RollupMode};
    use crate::database::DelegatedPermission;
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
        assert_eq!(
            cmds[0],
            Err(CommandParseError::UnknownCommand("foo".to_string()))
        );
    }

    #[test]
    fn parse_arg_no_value() {
        let cmds = parse_commands("@bors ping a=");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Err(CommandParseError::MissingArgValue {
                arg: "a".to_string()
            })
        );
    }

    #[test]
    fn parse_duplicate_key() {
        let cmds = parse_commands("@bors ping a=b a=c");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Err(CommandParseError::DuplicateArg("a".to_string()))
        );
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
    fn parse_approve_empty_reviewer() {
        let cmds = parse_commands("@bors r=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "r",
            },
        )
        "#);
    }

    #[test]
    fn parse_approve_space_after_r() {
        let cmds = parse_commands("@bors r= user1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "r",
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "#);
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "#);
    }

    #[test]
    fn parse_approve_priority_empty() {
        let cmds = parse_commands("@bors r+ p=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "p",
            },
        )
        "#);
    }

    #[test]
    fn parse_priority_exceeds_max_u32() {
        let cmds = parse_commands("@bors p=4294967296");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "#);
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "p",
            },
        )
        "#);
    }

    #[test]
    fn parse_priority_invalid() {
        let cmds = parse_commands("@bors p=abc");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "#);
    }

    #[test]
    fn parse_priority_negative() {
        let cmds = parse_commands("@bors p=-1");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Priority must be a non-negative integer",
            ),
        )
        "#);
    }

    #[test]
    fn parse_duplicate_priority() {
        let cmds = parse_commands("@bors p=1 p=2");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Err(CommandParseError::DuplicateArg("p".to_string()))
        );
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "rollup",
            },
        )
        "#);
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "rollup",
            },
        )
        "#);
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
        insta::assert_debug_snapshot!(cmds[0], @"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        ");
    }

    #[test]
    fn parse_try() {
        let cmds = parse_commands("@bors try");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        ");
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
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Try parent has to be a valid commit SHA: SHA must have exactly 40 characters",
            ),
        )
        "#);
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
    fn parse_try_jobs_glob() {
        let cmds = parse_commands("@bors try jobs=ci-1,lint_2,foo*");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: None,
                jobs: vec!["ci-1".to_string(), "lint_2".to_string(), "foo*".to_string()]
            })
        );
    }

    // Make sure that *foo* gets parsed as foo, ignoring the Markdown italics.
    #[test]
    fn parse_try_jobs_glob_2() {
        let cmds = parse_commands("@bors try jobs=*x86_64-msvc*");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: None,
                jobs: vec!["*x86_64-msvc*".to_string()]
            })
        );
    }

    // Make sure that foo\\* gets parsed as foo*, so that people can escape * to avoid weird
    // rendering on GitHub.
    #[test]
    fn parse_try_jobs_glob_3() {
        let cmds = parse_commands("@bors try jobs=\\*x86_64-msvc\\*,foo\\*");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: None,
                jobs: vec!["*x86_64-msvc*".to_string(), "foo*".to_string()]
            })
        );
    }

    #[test]
    fn parse_try_jobs_empty() {
        let cmds = parse_commands("@bors try jobs=");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            MissingArgValue {
                arg: "jobs",
            },
        )
        "#);
    }

    #[test]
    fn parse_try_jobs_too_many() {
        let cmds =
            parse_commands("@bors try jobs=ci,lint,foo,bar,baz,qux,quux,corge,grault,garply,waldo");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Try jobs must not have more than 10 jobs",
            ),
        )
        "#);
    }

    #[test]
    fn parse_try_md_paragraph() {
        let cmds = parse_commands(
            "@bors try

for the crater",
        );
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        ");
    }

    #[test]
    fn parse_try_unknown_arg() {
        let cmds = parse_commands("@bors try a");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Err(CommandParseError::UnknownArg("a".to_string())));
    }

    #[test]
    fn parse_try_unknown_kv_arg() {
        let cmds = parse_commands("@bors try a=b");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Err(CommandParseError::UnknownArg("a".to_string())));
    }

    #[test]
    fn parse_try_with_rust_timer() {
        let cmds = parse_commands(
            r#"
@bors try @rust-timer queue
"#,
        );
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @"
        Ok(
            Try {
                parent: None,
                jobs: [],
            },
        )
        ")
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
    fn parse_retry() {
        let cmds = parse_commands("@bors retry");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Retry)));
    }

    #[test]
    fn parse_retry_unknown_arg() {
        let cmds = parse_commands("@bors retry xyz");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::Retry)));
    }

    #[test]
    fn parse_try_cancel() {
        let cmds = parse_commands("@bors try cancel");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(cmds[0], Ok(BorsCommand::TryCancel)));
    }

    #[test]
    fn parse_delegate_plus_author() {
        let cmds = parse_commands("@bors delegate+");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Ok(BorsCommand::SetDelegate(DelegatedPermission::Review))
        ));
    }

    #[test]
    fn parse_delegate_author() {
        let cmds = parse_commands("@bors delegate");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Ok(BorsCommand::SetDelegate(DelegatedPermission::Review))
        ));
    }

    #[test]
    fn parse_delegate_review_author() {
        let cmds = parse_commands("@bors delegate=review");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Ok(BorsCommand::SetDelegate(DelegatedPermission::Review))
        ));
    }

    #[test]
    fn parse_delegate_try_author() {
        let cmds = parse_commands("@bors delegate=try");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Ok(BorsCommand::SetDelegate(DelegatedPermission::Try))
        ));
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
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::SetDelegate(DelegatedPermission::Review))
        );
    }

    #[test]
    fn parse_delegate_invalid_value() {
        let cmds = parse_commands("@bors delegate=invalid");
        assert_eq!(cmds.len(), 1);
        insta::assert_debug_snapshot!(cmds[0], @r#"
        Err(
            ValidationError(
                "Invalid delegation type `invalid`. Possible values are try/review",
            ),
        )
        "#);
    }

    #[test]
    fn parse_tree_closed() {
        let cmds = parse_commands("@bors treeclosed=5");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::TreeClosed(5)));
    }

    #[test]
    fn parse_tree_closed_invalid() {
        let cmds = parse_commands("@bors treeclosed=abc");
        assert_eq!(cmds.len(), 1);
        assert!(matches!(
            cmds[0],
            Err(CommandParseError::ValidationError(_))
        ));
    }

    #[test]
    fn parse_tree_closed_empty() {
        let cmds = parse_commands("@bors treeclosed=");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Err(CommandParseError::MissingArgValue {
                arg: "treeclosed".to_string()
            })
        );
    }

    #[test]
    fn parse_tree_closed_minus() {
        let cmds = parse_commands("@bors treeclosed-");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::OpenTree));
    }

    #[test]
    fn parse_tree_closed_minus_alias() {
        let cmds = parse_commands("@bors treeopen");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::OpenTree));
    }

    #[test]
    fn parse_tree_closed_unknown_command() {
        let cmds = parse_commands("@bors tree closed 5");
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Err(CommandParseError::UnknownCommand("tree".to_string()))
        );
    }

    #[test]
    fn parse_cancel() {
        let cmds = parse_commands("@bors cancel");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::Cancel));
    }

    #[test]
    fn parse_yield() {
        let cmds = parse_commands("@bors yield");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0], Ok(BorsCommand::Cancel));
    }

    #[test]
    fn parse_in_html_command() {
        let cmds = parse_commands(
            r#"
<!--
I am markdown HTML comment
@bors try
-->
"#,
        );
        assert_eq!(cmds.len(), 1);
        assert_eq!(
            cmds[0],
            Ok(BorsCommand::Try {
                parent: None,
                jobs: vec![]
            })
        );
    }

    #[test]
    fn ignore_markdown_codeblock() {
        let cmds = parse_commands(
            r#"
```rust
@bors try
```
"#,
        );
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn ignore_markdown_backtikcs() {
        let cmds = parse_commands("Don't do `@bors try`!");
        assert_eq!(cmds.len(), 0);
    }

    #[test]
    fn ignore_markdown_link() {
        let cmds = parse_commands("Ignore [me](@bors-try).");
        assert_eq!(cmds.len(), 0);
    }

    fn parse_commands(text: &str) -> Vec<Result<BorsCommand, CommandParseError>> {
        CommandParser::new("@bors".to_string().into()).parse_commands(text)
    }
}
