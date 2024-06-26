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

#[derive(Clone, Debug, PartialEq)]
pub enum Approver {
    /// The approver is the same as the comment author.
    Myself,
    /// The approver is specified by the user.
    Specified(String),
}

/// Bors command specified by a user.
#[derive(Debug, PartialEq)]
pub enum BorsCommand {
    /// Approve a commit.
    Approve(Approver),
    /// Unapprove a commit.
    Unapprove,
    /// Print help
    Help,
    /// Ping the bot.
    Ping,
    /// Perform a try build.
    Try {
        /// Parent commit which should be used as the merge base.
        parent: Option<Parent>,
        /// The CI workflow to run.
        jobs: Vec<String>,
    },
    /// Cancel a try build.
    TryCancel,
}
