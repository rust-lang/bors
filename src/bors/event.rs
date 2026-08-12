use crate::database::{WorkflowStatus, WorkflowType};
use crate::github::{CommitSha, GithubRepoName, GithubUser, PullRequest, PullRequestNumber};
use chrono::Duration;
use octocrab::models::{CheckSuiteId, JobId, RunId};

#[derive(Debug)]
pub enum BorsRepositoryEvent {
    /// A comment was posted on a pull request.
    Comment(PullRequestComment),
    /// When a new commit is pushed to the pull request branch.
    PullRequestCommitPushed(PullRequestPushed),
    /// When the pull request is edited by its author
    PullRequestEdited(PullRequestEdited),
    /// When a pull request is opened.
    PullRequestOpened(PullRequestOpened),
    /// When a pull request is closed.
    PullRequestClosed(PullRequestClosed),
    // When a pull request is merged
    PullRequestMerged(PullRequestMerged),
    // When a pull request is reopened
    PullRequestReopened(PullRequestReopened),
    // When a pull request is converted to draft
    PullRequestConvertedToDraft(PullRequestConvertedToDraft),
    // When a pull request is assigned to a user
    PullRequestAssigned(PullRequestAssigned),
    // When a pull request is unassigned from a user
    PullRequestUnassigned(PullRequestUnassigned),
    // When a pull request is ready for review
    PullRequestReadyForReview(PullRequestReadyForReview),
    /// When there is a push to a branch. This includes when a commit is pushed, when a commit tag is pushed,
    /// when a branch is deleted or when a tag is deleted.
    PushToBranch(PushToBranch),
    /// A workflow run on Github Actions or a check run from external CI system has started.
    WorkflowStarted(WorkflowRunStarted),
    /// A workflow run on Github Actions or a check run from external CI system has completed.
    WorkflowCompleted(WorkflowRunCompleted),
    /// A workflow job on Github Actions has started.
    WorkflowJobStarted(WorkflowJobStarted),
    /// A workflow job on Github Actions has completed.
    WorkflowJobCompleted(WorkflowJobCompleted),
}

impl BorsRepositoryEvent {
    pub fn repository(&self) -> &GithubRepoName {
        match self {
            BorsRepositoryEvent::Comment(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestCommitPushed(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestEdited(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestOpened(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestClosed(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestMerged(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestReopened(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestConvertedToDraft(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestAssigned(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestUnassigned(payload) => &payload.repository,
            BorsRepositoryEvent::PullRequestReadyForReview(payload) => &payload.repository,
            BorsRepositoryEvent::PushToBranch(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowStarted(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowCompleted(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowJobStarted(payload) => &payload.repository,
            BorsRepositoryEvent::WorkflowJobCompleted(payload) => &payload.repository,
        }
    }
}

#[derive(Debug)]
pub enum BorsGlobalEvent {
    /// The configuration of some repository has been changed for the bot's Github App.
    InstallationsChanged,
    /// Refresh the configuration of each tracked repository.
    RefreshConfig,
    /// Refresh the team permissions.
    RefreshPermissions,
    /// Examine pending builds, and try to complete or timeout them.
    RefreshPendingBuilds,
    /// Refresh mergeability status of PRs that have unknown mergeability status.
    RefreshPullRequestMergeability,
    /// Synchronize PR status with GitHub.
    RefreshPullRequestState,
    /// Try to process the merge queue.
    ProcessMergeQueue,
    /// Try to terminate old EC2 running instances.
    TerminateOldEC2Instances,
    /// Try to create EC2 instances for jobs that have been queued for some time.
    BackfillEC2Instances,
    /// Reload jobs of pending workfows into the in-memory job cache.
    ReloadWorkflowJobCache,
    /// Start or complete unrolled builds of rollups.
    ProcessUnrolledMemberBuilds,
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum BorsEvent {
    /// An event that happen per repository basis.
    Repository(BorsRepositoryEvent),
    /// An event that happen with bors globally.
    Global(BorsGlobalEvent),
}

#[derive(Debug)]
pub struct PullRequestComment {
    pub repository: GithubRepoName,
    pub author: GithubUser,
    pub pr_number: PullRequestNumber,
    pub text: String,
    pub html_url: String,
}

#[derive(Debug)]
pub struct PullRequestPushed {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestEdited {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
    pub from_base_sha: Option<CommitSha>,
}

#[derive(Debug)]
pub struct PullRequestOpened {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
    pub draft: bool,
}

#[derive(Debug)]
pub struct PullRequestClosed {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestMerged {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestReopened {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestConvertedToDraft {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestReadyForReview {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestAssigned {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PullRequestUnassigned {
    pub repository: GithubRepoName,
    pub pull_request: PullRequest,
}

#[derive(Debug)]
pub struct PushToBranch {
    pub repository: GithubRepoName,
    pub branch: String,
    pub sha: String,
}

#[derive(Debug)]
pub struct WorkflowRunStarted {
    pub repository: GithubRepoName,
    pub name: String,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
    pub workflow_type: WorkflowType,
    pub url: String,
}

#[derive(Debug)]
pub struct WorkflowRunCompleted {
    pub repository: GithubRepoName,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
    pub status: WorkflowStatus,
    pub running_time: Option<Duration>,
    /// Check suite to which this workflow is attached.
    pub check_suite_id: CheckSuiteId,
}

#[derive(Debug)]
pub struct WorkflowJobStarted {
    pub repository: GithubRepoName,
    pub job_id: JobId,
    pub name: String,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
    pub labels: Vec<String>,
}

#[derive(Debug)]
pub struct WorkflowJobCompleted {
    pub repository: GithubRepoName,
    pub job_id: JobId,
    pub name: String,
    pub branch: String,
    pub commit_sha: CommitSha,
    pub run_id: RunId,
}
