mod parser;
use std::str::FromStr;

use crate::github::CommitSha;
pub use parser::{CommandParseError, CommandParser};

/// Priority of a commit.
pub type Priority = u32;

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

const ROLLUP_VALUES: [&str; 4] = ["always", "iffy", "maybe", "never"];

#[derive(Clone, Debug, PartialEq)]
pub enum RollupMode {
    Always,
    Iffy,
    Maybe,
    Never,
}

impl ToString for RollupMode {
    fn to_string(&self) -> String {
        match self {
            RollupMode::Always => "always".to_string(),
            RollupMode::Iffy => "iffy".to_string(),
            RollupMode::Never => "never".to_string(),
            RollupMode::Maybe => "maybe".to_string(),
        }
    }
}

impl<'a> FromStr for RollupMode {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "always" => Ok(RollupMode::Always),
            "iffy" => Ok(RollupMode::Iffy),
            "never" => Ok(RollupMode::Never),
            "maybe" => Ok(RollupMode::Maybe),
            _ => Err(()),
        }
    }
}

/// Bors command specified by a user.
#[derive(Debug, PartialEq)]
pub enum BorsCommand {
    /// Approve a commit.
    Approve {
        /// Who is approving the commit.
        approver: Approver,
        /// Priority of the commit.
        priority: Option<Priority>,
        // Rollup status of the commit.
        rollup: Option<RollupMode>,
    },
    /// Unapprove a commit.
    Unapprove,
    /// Print help.
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
    /// Set the priority of a commit.
    SetPriority(Priority),
    /// Delegate approval authority to the pull request author.
    Delegate,
    /// Revoke any previously granted delegation.
    Undelegate,
    /// Rollup status.
    Rollup(RollupMode),
}
