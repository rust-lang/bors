use crate::database::DbClient;
use crate::github::api::client::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};
use axum::async_trait;
use entity::try_build;
use migration::sea_orm::DatabaseConnection;
use sea_orm::ActiveValue::Set;
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter};

pub struct SeaORMClient {
    db: DatabaseConnection,
}

impl SeaORMClient {
    pub fn new(connection: DatabaseConnection) -> Self {
        Self { db: connection }
    }
}

#[async_trait]
impl DbClient for SeaORMClient {
    async fn insert_try_build(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        commit: CommitSha,
    ) -> anyhow::Result<()> {
        let model = try_build::ActiveModel {
            repository: Set(full_repo_name(repo)),
            pull_request_number: Set(pr_number.0 as i32),
            commit_sha: Set(commit.0),
            ..Default::default()
        };
        model.insert(&self.db).await?;
        Ok(())
    }

    async fn find_try_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit: &CommitSha,
    ) -> anyhow::Result<Option<try_build::Model>> {
        let mut entries = try_build::Entity::find()
            .filter(
                try_build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(try_build::Column::CommitSha.eq(&commit.0)),
            )
            .all(&self.db)
            .await?;
        Ok(entries.pop())
    }

    async fn find_try_build_for_pr(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<Option<try_build::Model>> {
        let mut entries = try_build::Entity::find()
            .filter(
                try_build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(try_build::Column::PullRequestNumber.eq(pr_number.0)),
            )
            .all(&self.db)
            .await?;
        Ok(entries.pop())
    }

    async fn delete_try_build(&self, model: try_build::Model) -> anyhow::Result<()> {
        try_build::Entity::delete_by_id(model.id)
            .exec(&self.db)
            .await?;
        Ok(())
    }
}

fn full_repo_name(repo: &GithubRepoName) -> String {
    format!("{}/{}", repo.owner(), repo.name())
}
