mod parser;
use std::fmt;
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

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RollupMode {
    Always,
    Iffy,
    Maybe,
    Never,
}

impl fmt::Display for RollupMode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = match self {
            RollupMode::Always => "always",
            RollupMode::Iffy => "iffy",
            RollupMode::Never => "never",
            RollupMode::Maybe => "maybe",
        };
        write!(f, "{}", s)
    }
}

// Has to be kept in sync with the `Display` implementation above.
impl FromStr for RollupMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "always" => Ok(RollupMode::Always),
            "iffy" => Ok(RollupMode::Iffy),
            "never" => Ok(RollupMode::Never),
            "maybe" => Ok(RollupMode::Maybe),
            _ => Err(format!(
                "Invalid rollup mode `{s}`. Possible values are always/iffy/never/maybe"
            )),
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
    /// Set the priority of a PR.
    SetPriority(Priority),
    /// Get information about the current PR.
    Info,
    /// Delegate approval authority to the pull request author.
    Delegate,
    /// Revoke any previously granted delegation.
    Undelegate,
    /// Set the rollup mode of a PRstatus.
    SetRollupMode(RollupMode),
    /// Open the repository tree for merging.
    OpenTree,
    /// Set the tree closed with a priority level.
    TreeClosed(Priority),
}
