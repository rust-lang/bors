mod parser;
use crate::github::CommitSha;
pub use parser::{CommandParseError, CommandParser};

/// Type of parent allowed in a try build
#[derive(Clone, Debug, PartialEq)]
pub enum Parent {
    /// Regular commit sha: parent="<sha>"
    CommitSha(CommitSha),
    /// Use last build's parent: parent="last"
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
        /// The CI workflow to run.
        jobs: Option<Vec<String>>,
    },
    /// Cancel a try build.
    TryCancel,
}
