//! Provides access to the database.
//! It should be as self-contained as possible and it should ideally not expose the used database
//! (ORM) types, to avoid dependencies on the ORM in the rest of the codebase.
//!
//! That is why the database model types are explicitly specified here, instead of just using the
//! ORM (currently SeaORM) types.
use axum::async_trait;
use chrono::{DateTime, Utc};
use octocrab::models::RunId;

pub use sea_orm_client::SeaORMClient;

use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

mod sea_orm_client;

type PrimaryKey = i32;

/// Status of a GitHub build.
#[derive(Debug, PartialEq)]
pub enum BuildStatus {
    /// The build is still waiting for results.
    Pending,
    /// The build has succeeded.
    Success,
    /// The build has failed.
    Failure,
    /// The build has been manually cancelled by a user.
    Cancelled,
}

/// Represents a single (merged) commit.
pub struct BuildModel {
    pub id: PrimaryKey,
    pub repository: String,
    pub branch: String,
    pub commit_sha: String,
    pub status: BuildStatus,
    pub created_at: DateTime<Utc>,
}

/// Represents a pull request.
pub struct PullRequestModel {
    pub id: PrimaryKey,
    pub repository: String,
    pub number: PullRequestNumber,
    pub try_build: Option<BuildModel>,
    pub created_at: DateTime<Utc>,
}

/// Describes whether a workflow is a Github Actions workflow or if it's a job from some external
/// CI.
#[derive(Debug, PartialEq)]
pub enum WorkflowType {
    Github,
    External,
}

/// Status of a workflow.
#[derive(Debug, PartialEq)]
pub enum WorkflowStatus {
    /// Workflow is running.
    Pending,
    /// Workflow has succeeded.
    Success,
    /// Workflow has failed.
    Failure,
}

/// Represents a workflow run, coming either from Github Actions or from some external CI.
pub struct WorkflowModel {
    pub id: PrimaryKey,
    pub build: BuildModel,
    pub name: String,
    pub url: String,
    pub run_id: RunId,
    pub workflow_type: WorkflowType,
    pub status: WorkflowStatus,
    pub created_at: DateTime<Utc>,
}

/// Provides access to a database.
#[async_trait]
pub trait DbClient {
    /// Finds a Pull request row for the given repository and PR number.
    /// If it doesn't exist, a new row is created.
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<PullRequestModel>;

    /// Finds a Pull request by a build (either a try or merge one).
    async fn find_pr_by_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Option<PullRequestModel>>;

    /// Attaches an existing build to the given PR.
    async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()>;

    /// Finds a Build row by its repository, commit SHA and branch.
    async fn find_build(
        &self,
        repo: &GithubRepoName,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>>;

    /// Updates the status of this build in the DB.
    async fn update_build_status(
        &self,
        build: &BuildModel,
        status: BuildStatus,
    ) -> anyhow::Result<()>;

    /// Creates a new workflow attached to a build.
    async fn create_workflow(
        &self,
        build: &BuildModel,
        name: String,
        url: String,
        run_id: RunId,
        workflow_type: WorkflowType,
        status: WorkflowStatus,
    ) -> anyhow::Result<()>;

    /// Updates the status of a workflow with the given run ID in the DB.
    async fn update_workflow_status(
        &self,
        run_id: u64,
        status: WorkflowStatus,
    ) -> anyhow::Result<()>;

    /// Get all workflows attached to a build.
    async fn get_workflows_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<WorkflowModel>>;
}
