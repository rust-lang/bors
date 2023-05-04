use axum::async_trait;
use sea_orm::Database;

use entity::try_build::Model;
use migration::{Migrator, MigratorTrait};

use crate::database::{DbClient, SeaORMClient};
use crate::github::{CommitSha, GithubRepoName};

pub struct TestDbClient {
    client: SeaORMClient,
}

#[async_trait]
impl DbClient for TestDbClient {
    async fn insert_try_build(
        &self,
        repo: &GithubRepoName,
        pr_number: u64,
        commit: CommitSha,
    ) -> anyhow::Result<()> {
        self.client.insert_try_build(repo, pr_number, commit).await
    }

    async fn find_try_build(
        &self,
        repo: &GithubRepoName,
        commit: &CommitSha,
    ) -> anyhow::Result<Option<Model>> {
        self.client.find_try_build(repo, commit).await
    }

    async fn delete_try_build(&self, model: Model) -> anyhow::Result<()> {
        self.client.delete_try_build(model).await
    }
}

pub async fn create_test_db() -> TestDbClient {
    let db = Database::connect("sqlite::memory:").await.unwrap();
    Migrator::up(&db, None).await.unwrap();
    TestDbClient {
        client: SeaORMClient::new(db.clone()),
    }
}
