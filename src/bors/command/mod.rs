mod parser;
use crate::github::CommitSha;
pub use parser::{CommandParseError, CommandParser};

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
        parent: Option<CommitSha>,
    },
    /// Cancel a try build.
    TryCancel,
}
