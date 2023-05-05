use axum::async_trait;
use entity::{build, pull_request};

mod sea_orm_client;

use crate::github::api::client::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};
pub use sea_orm_client::SeaORMClient;

#[async_trait]
pub trait DbClient {
    /// Finds a Pull request row for the given repository and PR number.
    /// If it doesn't exist, a new row is created.
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<pull_request::Model>;

    /// Creates a new Build row and attaches it as a try build to the given PR.
    async fn attach_try_build(
        &self,
        pr: pull_request::Model,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()>;

    /// Finds a new Build row by its commit SHA.
    async fn find_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<build::Model>>;

    // async fn find_try_build_by_commit(
    //     &self,
    //     repo: &GithubRepoName,
    //     commit: &CommitSha,
    // ) -> anyhow::Result<Option<try_build::Model>>;
    //
    // async fn find_try_build_for_pr(
    //     &self,
    //     repo: &GithubRepoName,
    //     pr_number: PullRequestNumber,
    // ) -> anyhow::Result<Option<try_build::Model>>;
    //
    // async fn delete_try_build(&self, model: try_build::Model) -> anyhow::Result<()>;
}
