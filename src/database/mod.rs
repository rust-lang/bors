use axum::async_trait;
use entity::try_build;

mod sea_orm_client;

use crate::github::{CommitSha, GithubRepoName};
pub use sea_orm_client::SeaORMClient;

#[async_trait]
pub trait DbClient {
    async fn insert_try_build(
        &self,
        repo: &GithubRepoName,
        pr_number: u64,
        commit: CommitSha,
    ) -> anyhow::Result<()>;

    async fn find_try_build(
        &self,
        repo: &GithubRepoName,
        commit: &CommitSha,
    ) -> anyhow::Result<Option<try_build::Model>>;

    async fn delete_try_build(&self, model: try_build::Model) -> anyhow::Result<()>;
}
