use crate::config::RepositoryConfig;
use crate::github::GithubRepoName;
use crate::github::api::client::{CommitAuthor, GithubRepositoryClient};
use crate::permissions::UserPermissions;
#[cfg(test)]
use crate::tests::TestSyncMarker;
use arc_swap::ArcSwap;
use chrono::{DateTime, Utc};
pub use command::CommandParser;
pub use command::RollupMode;
pub use comment::Comment;
pub use context::BorsContext;
pub use handlers::{handle_bors_global_event, handle_bors_repository_event};
use itertools::Itertools;
use octocrab::models::RunId;
use octocrab::models::workflows::Job;
use regex::{Regex, RegexBuilder};
use serde::Serialize;
use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;
use std::sync::{Arc, LazyLock, RwLock};
use std::time::Duration;

mod build;
mod build_queue;
mod command;
pub mod comment;
mod context;
pub mod event;
mod handlers;
mod labels;
pub mod merge_queue;
pub mod mergeability_queue;
pub mod process;

use crate::bors::command::BorsCommand;
use crate::database::WorkflowStatus;
pub use command::CommandPrefix;

/// Branch where CI checks run for auto builds.
/// This branch should run CI checks.
pub const AUTO_BRANCH_NAME: &str = "automation/bors/auto";

/// This branch should run CI checks.
pub const TRY_BRANCH_NAME: &str = "automation/bors/try";

#[derive(PartialEq, Eq, Copy, Clone, Debug)]
pub enum BuildKind {
    Try,
    Auto,
}

/// Format the bors command help in Markdown format.
pub fn format_help() -> &'static str {
    // The help is generated manually to have a nicer structure.
    // We do a no-op destructuring of `BorsCommand` to make it harder to modify help in case new
    // commands are added though.
    match BorsCommand::Ping {
        BorsCommand::Approve {
            approver: _,
            rollup: _,
            priority: _,
        } => {}
        BorsCommand::Unapprove => {}
        BorsCommand::Help => {}
        BorsCommand::Ping => {}
        BorsCommand::Try { parent: _, jobs: _ } => {}
        BorsCommand::TryCancel => {}
        BorsCommand::SetPriority(_) => {}
        BorsCommand::Info => {}
        BorsCommand::SetDelegate(_) => {}
        BorsCommand::Undelegate => {}
        BorsCommand::SetRollupMode(_) => {}
        BorsCommand::OpenTree => {}
        BorsCommand::TreeClosed(_) => {}
        BorsCommand::Retry => {}
        BorsCommand::Cancel => {}
    }

    r#"
You can use the following commands:

## PR management
- `r+ [p=<priority>] [rollup=<never|iffy|maybe|always>]`: Approve this PR on your behalf
    - Optionally, you can specify the `<priority>` of the PR and if it is eligible for rollups (`<rollup>)`.
- `r=<user> [p=<priority>] [rollup=<never|iffy|maybe|always>]`: Approve this PR on behalf of `<user>`
    - Optionally, you can specify the `<priority>` of the PR and if it is eligible for rollups (`<rollup>)`.
    - You can pass a comma-separated list of GitHub usernames.
- `r-`: Unapprove this PR
- `p=<priority>` or `priority=<priority>`: Set the priority of this PR
- `rollup=<never|iffy|maybe|always>`: Set the rollup status of the PR
- `rollup`: Short for `rollup=always`
- `rollup-`: Short for `rollup=maybe`
- `delegate=<try|review>`: Delegate permissions for running try builds or approving to the PR author
    - `try` allows the PR author to start try builds.
    - `review` allows the PR author to both start try builds and approve the PR.
- `delegate+`: Delegate approval permissions to the PR author
    - Shortcut for `delegate=review`
- `delegate-`: Remove any previously granted permission delegation
- `try [parent=<parent>] [job|jobs=<jobs>]`: Start a try build.
    - Optionally, you can specify a `<parent>` SHA with which will the PR be merged. You can specify `parent=last` to use the same parent SHA as the previous try build.
    - Optionally, you can select a comma-separated list of CI `<jobs>` to run in the try build.
- `try cancel`: Cancel a running try build on the current PR.
- `retry`: Clear a failed auto build status from an approved PR. This will cause the merge queue to eventually attempt to merge the PR again.
- `cancel` | `yield`: Cancel a running auto build on the current PR.
- `info`: Get information about the current PR

## Repository management
- `treeclosed=<priority>`: Close the tree for PRs with priority less than `<priority>`
- `treeclosed-` or `treeopen`: Open the repository tree for merging

## Meta commands
- `ping`: Check if the bot is alive
- `help`: Print this help message
"#
}

#[cfg(test)]
pub static WAIT_FOR_BUILD_QUEUE: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_MERGEABILITY_STATUS_REFRESH: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_PR_STATUS_REFRESH: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_STARTED: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_WORKFLOW_COMPLETED: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_PR_OPEN: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_MERGE_QUEUE: TestSyncMarker = TestSyncMarker::new();

#[cfg(test)]
pub static WAIT_FOR_COMMENTS_HANDLED: TestSyncMarker = TestSyncMarker::new();

/// The merge queue has attempted to merge a PR.
#[cfg(test)]
pub static WAIT_FOR_MERGE_QUEUE_MERGE_ATTEMPT: TestSyncMarker = TestSyncMarker::new();

#[cfg(not(test))]
fn now() -> DateTime<Utc> {
    Utc::now()
}

#[cfg(test)]
thread_local! {
    static MOCK_TIME: std::cell::RefCell<Option<DateTime<Utc>>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn now() -> DateTime<Utc> {
    MOCK_TIME.with(|time| time.borrow_mut().unwrap_or_else(Utc::now))
}

#[cfg(test)]
async fn with_mocked_time<Fut: Future<Output = R>, R>(in_future: Duration, future: Fut) -> R {
    use tokio::runtime::RuntimeFlavor;

    // It is important to use this function only with a single threaded runtime,
    // otherwise the `MOCK_TIME` variable might get mixed up between different threads.
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        RuntimeFlavor::CurrentThread
    );
    MOCK_TIME.with(|time| {
        *time.borrow_mut() = Some(Utc::now() + chrono::Duration::from_std(in_future).unwrap());
    });
    let res = future.await;
    MOCK_TIME.with(|time| {
        *time.borrow_mut() = None;
    });
    res
}

fn elapsed_time_since(date: DateTime<Utc>) -> Duration {
    let time: DateTime<Utc> = now();
    (time - date).to_std().unwrap_or(Duration::ZERO)
}

/// Corresponds to a single execution of a workflow.
#[derive(Clone, Debug)]
pub struct WorkflowRun {
    pub id: RunId,
    pub name: String,
    pub url: String,
    pub status: WorkflowStatus,
}

pub struct FailedWorkflowRun {
    pub workflow_run: WorkflowRun,
    pub failed_jobs: Vec<Job>,
}

/// An access point to a single repository.
/// Can be used to query permissions for the repository, and also to perform various
/// actions using the stored client.
pub struct RepositoryState {
    pub client: GithubRepositoryClient,
    pub permissions: ArcSwap<UserPermissions>,
    pub config: ArcSwap<RepositoryConfig>,
}

impl RepositoryState {
    pub fn repository(&self) -> &GithubRepoName {
        self.client.repository()
    }
}

#[derive(Default)]
pub struct RepositoryStore {
    repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState>>>,
}

impl RepositoryStore {
    pub fn get(&self, name: &GithubRepoName) -> Option<Arc<RepositoryState>> {
        self.repositories.read().unwrap().get(name).cloned()
    }

    /// Adds a new repository to the store or updates the previous one.
    /// Returns true if a new repository was added.
    pub fn insert(&self, repo: RepositoryState) -> bool {
        self.repositories
            .write()
            .unwrap()
            .insert(repo.repository().clone(), Arc::new(repo))
            .is_none()
    }
    pub fn repo_count(&self) -> usize {
        self.repositories.read().unwrap().len()
    }
    pub fn repositories(&self) -> Vec<Arc<RepositoryState>> {
        self.repositories
            .read()
            .unwrap()
            .values()
            .cloned()
            .collect()
    }
    pub fn repository_names(&self) -> Vec<GithubRepoName> {
        self.repositories.read().unwrap().keys().cloned().collect()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum PullRequestStatus {
    Closed,
    Draft,
    Merged,
    Open,
}

impl fmt::Display for PullRequestStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let status_str = match self {
            PullRequestStatus::Closed => "closed",
            PullRequestStatus::Draft => "draft",
            PullRequestStatus::Merged => "merged",
            PullRequestStatus::Open => "open",
        };
        write!(f, "{status_str}")
    }
}

// Has to be kept in sync with the `Display` implementation above.
impl FromStr for PullRequestStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "closed" => Ok(PullRequestStatus::Closed),
            "draft" => Ok(PullRequestStatus::Draft),
            "merged" => Ok(PullRequestStatus::Merged),
            "open" => Ok(PullRequestStatus::Open),
            status => Err(format!("Invalid PR status {status}")),
        }
    }
}

#[derive(Debug, Clone)]
pub enum MergeType {
    Try { try_jobs: Vec<String> },
    Auto,
}

/// Commit author used to emulate old bors (homu).
fn bors_commit_author() -> CommitAuthor {
    CommitAuthor {
        name: "bors".to_string(),
        email: "bors@rust-lang.org".to_string(),
    }
}

/// HTML comment that marks the start of a bors ignore block.
/// The block is called "homu" to maintain compatibility with the old bors implementation
/// (called homu).
const IGNORE_BLOCK_START: &str = "<!-- homu-ignore:start -->";
/// HTML comment that marks the end of a bors ignore block.
const IGNORE_BLOCK_END: &str = "<!-- homu-ignore:end -->";

/// Returns a string with the given content that will be ignored by bors on GitHub.
pub fn make_text_ignored_by_bors(text: &str) -> String {
    format!("{IGNORE_BLOCK_START}\n{text}\n{IGNORE_BLOCK_END}")
}

/// Remove homu-ignore blocks from the merge message
pub fn normalize_merge_message(message: &str) -> String {
    static IGNORE_REGEX: LazyLock<Regex> = LazyLock::new(|| {
        RegexBuilder::new(r"<!--\s*homu-ignore:start\s*-->.*?<!--\s*homu-ignore:end\s*-->")
            .multi_line(true)
            .case_insensitive(true)
            .dot_matches_new_line(true)
            .build()
            .unwrap()
    });
    IGNORE_REGEX.replace_all(message, "").to_string()
}

pub fn create_merge_commit_message(pr: handlers::PullRequestData, merge_type: MergeType) -> String {
    /// Prefix used to specify custom try jobs in PR descriptions.
    const CUSTOM_TRY_JOB_PREFIX: &str = "try-job:";

    let pr_number = pr.number();

    let reviewer = match &merge_type {
        MergeType::Try { .. } => "<try>",
        MergeType::Auto => pr.db.approver().unwrap_or("<unknown>"),
    };

    let pr_description = match &merge_type {
        // Only keep any lines starting with `CUSTOM_TRY_JOB_PREFIX`.
        // If we do not have any custom try jobs, keep the ones that might be in the PR
        // description.
        MergeType::Try { try_jobs } if try_jobs.is_empty() => pr
            .github
            .message
            .lines()
            .map(|l| l.trim())
            .filter(|l| l.starts_with(CUSTOM_TRY_JOB_PREFIX))
            .join("\n"),
        // If we do have custom jobs, ignore the original description completely
        MergeType::Try { .. } => String::new(),
        MergeType::Auto => pr.github.message.clone(),
    };
    let pr_description = normalize_merge_message(&pr_description);

    let mut message = format!(
        r#"Auto merge of #{pr_number} - {pr_label}, r={reviewer}

{pr_title}

{pr_description}"#,
        pr_label = pr.github.head_label,
        pr_title = pr.github.title,
    );

    match merge_type {
        MergeType::Try { try_jobs } => {
            for job in try_jobs {
                message.push_str(&format!("\n{CUSTOM_TRY_JOB_PREFIX} {job}"));
            }
        }
        MergeType::Auto => {}
    }
    message
}
