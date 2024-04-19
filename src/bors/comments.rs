use std::fmt;

use super::{command::CommandParseError, CommitSha};

pub trait Comment: Send + Sync {
    fn render(&self) -> String;
}

pub struct TryBuildStartedComment {
    pub head_sha: CommitSha,
    pub merge_sha: CommitSha,
}

impl Comment for TryBuildStartedComment {
    fn render(&self) -> String {
        format!(
            ":hourglass: Trying commit {} with merge {}…",
            self.head_sha, self.merge_sha
        )
    }
}

pub struct TryBuildSuccessfulComment {
    pub sha: CommitSha,
    pub workflow_list: String,
}

impl Comment for TryBuildSuccessfulComment {
    fn render(&self) -> String {
        format!(
            r#":sunny: Try build successful
{}
Build commit: {} (`{}`)"#,
            self.workflow_list, self.sha, self.sha
        )
    }
}

pub struct TryBuildFailedComment {
    pub workflow_list: String,
}

impl Comment for TryBuildFailedComment {
    fn render(&self) -> String {
        format!(
            r#":broken_heart: Test failed
{}"#,
            self.workflow_list
        )
    }
}

pub struct MergeConflictComment {
    pub branch: String,
}

impl Comment for MergeConflictComment {
    fn render(&self) -> String {
        let branch = &self.branch;
        format!(
            r#":lock: Merge conflict

This pull request and the master branch diverged in a way that cannot
 be automatically merged. Please rebase on top of the latest master
 branch, and let the reviewer approve again.

<details><summary>How do I rebase?</summary>

Assuming `self` is your fork and `upstream` is this repository,
 you can resolve the conflict following these steps:

1. `git checkout {branch}` *(switch to your branch)*
2. `git fetch upstream master` *(retrieve the latest master)*
3. `git rebase upstream/master -p` *(rebase on top of it)*
4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
5. `git push self {branch} --force-with-lease` *(update this PR)*

You may also read
 [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
 for a short tutorial.

Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
 It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

</details>  
"#
        )
    }
}

pub struct TryBuildCancelledComment {
    pub workflow_urls: anyhow::Result<Vec<String>>,
}

impl Comment for TryBuildCancelledComment {
    fn render(&self) -> String {
        match &self.workflow_urls {
            Ok(workflow_urls) => {
                let mut try_build_cancelled_comment = r#"Try build cancelled.
Cancelled workflows:"#
                    .to_string();
                for url in workflow_urls {
                    try_build_cancelled_comment += format!("\n- {}", url).as_str();
                }
                try_build_cancelled_comment
            }
            Err(_) => {
                "Try build was cancelled. It was not possible to cancel some workflows.".to_string()
            }
        }
    }
}

#[derive(Default)]
pub struct TestTimedOutComment;
impl Comment for TestTimedOutComment {
    fn render(&self) -> String {
        ":boom: Test timed out".to_string()
    }
}

#[derive(Default)]
pub struct TryBuildInProgressComment;
impl Comment for TryBuildInProgressComment {
    fn render(&self) -> String {
        ":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.".to_string()
    }
}

#[derive(Default)]
pub struct NoPreviousBuildComment;
impl Comment for NoPreviousBuildComment {
    fn render(&self) -> String {
        ":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.".to_string()
    }
}

#[derive(Default)]
pub struct NoRunningBuildComment;
impl Comment for NoRunningBuildComment {
    fn render(&self) -> String {
        ":exclamation: There is currently no try build in progress.".to_string()
    }
}

pub struct CommandParseErrorComment {
    pub message: String,
}
impl fmt::Display for CommandParseErrorComment {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl<'a> From<CommandParseError<'a>> for CommandParseErrorComment {
    fn from(error: CommandParseError) -> Self {
        let message = match error {
            CommandParseError::MissingCommand => "Missing command.".to_string(),
            CommandParseError::UnknownCommand(command) => {
                format!(r#"Unknown command "{command}"."#)
            }
            CommandParseError::MissingArgValue { arg } => {
                format!(r#"Unknown value for argument "{arg}"."#)
            }
            CommandParseError::UnknownArg(arg) => {
                format!(r#"Unknown argument "{arg}"."#)
            }
            CommandParseError::DuplicateArg(arg) => {
                format!(r#"Argument "{arg}" found multiple times."#)
            }
            CommandParseError::ValidationError(error) => {
                format!("Invalid command: {error}")
            }
        };
        Self { message }
    }
}

impl Comment for CommandParseErrorComment {
    fn render(&self) -> String {
        format!("{}", self.message)
    }
}

pub struct InsufficientTryPermissionComment {
    pub username: String,
}
impl Comment for InsufficientTryPermissionComment {
    fn render(&self) -> String {
        format!(
            "@{}: :key: Insufficient privileges: not in try users",
            self.username
        )
    }
}

#[derive(Default)]
pub struct PingComment;
impl Comment for PingComment {
    fn render(&self) -> String {
        "Pong 🏓!".to_string()
    }
}

#[derive(Default)]
pub struct CommandExecuteErrorComment;
impl Comment for CommandExecuteErrorComment {
    fn render(&self) -> String {
        ":x: Encountered an error while executing command".to_string()
    }
}

#[derive(Default)]
pub struct HelpComment;
impl Comment for HelpComment {
    fn render(&self) -> String {
        r#"
- try: Execute the `try` CI workflow on this PR (without approving it for a merge).
- try cancel: Stop try builds on current PR.
- ping: pong
- help: Print this help message
"#
        .to_string()
    }
}
