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

#[async_trait]
pub trait DbClient {
    /// Finds a Pull request row for the given repository and PR number.
    /// If it doesn't exist, a new row is created.
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<PullRequestModel>;

    /// Creates a new Build row and attaches it as a try build to the given PR.
    async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()>;

    /// Finds a new Build row by its commit SHA.
    async fn find_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>>;
}
