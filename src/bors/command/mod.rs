mod parser;
use crate::github::CommitSha;
pub use parser::{CommandParseError, CommandParser};

#[derive(Clone, Debug, PartialEq)]
pub enum Parent {
    CommitSha(CommitSha),
    Last,
}

/// Bors command specified by a user.
#[derive(Debug, PartialEq)]
pub enum BorsCommand {
    /// Print help
    Help,
    /// Ping the bot.
    Ping,
    /// Perform a try build.
    Try {
        /// Parent commit which should be used as the merge base.
        parent: Option<Parent>,
    },
    /// Cancel a try build.
    TryCancel,
}
