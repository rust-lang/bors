use axum::async_trait;
use chrono::{DateTime, Utc};

pub use sea_orm_client::SeaORMClient;

use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

mod sea_orm_client;

type PrimaryKey = i32;

pub struct BuildModel {
    pub id: PrimaryKey,
    pub repository: String,
    pub branch: String,
    pub commit_sha: String,
    pub created_at: DateTime<Utc>,
}

pub struct PullRequestModel {
    pub id: PrimaryKey,
    pub repository: String,
    pub number: PullRequestNumber,
    pub try_build: Option<BuildModel>,
    pub created_at: DateTime<Utc>,
}

pub enum CheckSuiteStatus {
    Pending,
    Success,
    Failure,
}

pub struct CheckSuiteModel {
    pub id: PrimaryKey,
    pub build: BuildModel,
    pub check_suite_id: u64,
    pub workflow_run_id: Option<u64>,
    pub status: CheckSuiteStatus,
    pub created_at: DateTime<Utc>,
}

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

    /// Creates a new check suite attached to a build.
    async fn create_check_suite(
        &self,
        build: &BuildModel,
        check_suite_id: u64,
        workflow_run_id: u64,
        status: CheckSuiteStatus,
    ) -> anyhow::Result<()>;
}
