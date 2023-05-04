use axum::async_trait;
use entity::try_build;

mod sea_orm_client;

use crate::github::api::client::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};
pub use sea_orm_client::SeaORMClient;

#[async_trait]
pub trait DbClient {
    async fn insert_try_build(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        commit: CommitSha,
    ) -> anyhow::Result<()>;

    async fn find_try_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit: &CommitSha,
    ) -> anyhow::Result<Option<try_build::Model>>;

    async fn find_try_build_for_pr(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<Option<try_build::Model>>;

    async fn delete_try_build(&self, model: try_build::Model) -> anyhow::Result<()>;
}
