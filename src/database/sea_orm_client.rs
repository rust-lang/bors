use anyhow::anyhow;
use axum::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter};

use entity::{build, pull_request};
use migration::sea_orm::DatabaseConnection;

use crate::database::{BuildModel, DbClient, PullRequestModel};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

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
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<PullRequestModel> {
        let pr = pull_request::ActiveModel {
            repository: Set(full_repo_name(repo)),
            number: Set(pr_number.0 as i32),
            ..Default::default()
        };

        // Try to insert first
        match pull_request::Entity::insert(pr)
            .on_conflict(OnConflict::new().do_nothing().to_owned())
            .exec_without_returning(&self.db)
            .await
        {
            Ok(_) => {}
            Err(DbErr::RecordNotInserted) => {
                // The record was already in DB
            }
            Err(error) => return Err(error.into()),
        };

        // Resolve the PR row
        let (pr, build) = pull_request::Entity::find()
            .filter(
                pull_request::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(pull_request::Column::Number.eq(pr_number.0)),
            )
            .find_also_related(build::Entity)
            .one(&self.db)
            .await?
            .ok_or_else(|| anyhow!("Cannot find PR row"))?;

        Ok(PullRequestModel {
            id: pr.id,
            repository: pr.repository,
            number: PullRequestNumber(pr.number as u64),
            try_build: build.map(from_db_build),
            created_at: from_db_datetime(pr.created_at),
        })
    }

    async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()> {
        let build = build::ActiveModel {
            commit_sha: Set(commit_sha.0),
            repository: Set(pr.repository.clone()),
            ..Default::default()
        };
        let build = build::Entity::insert(build)
            .exec_with_returning(&self.db)
            .await
            .map_err(|error| anyhow!("Cannot insert build into DB: {error:?}"))?;

        let pr_model = pull_request::ActiveModel {
            id: Unchanged(pr.id),
            try_build: Set(Some(build.id)),
            ..Default::default()
        };
        pr_model.update(&self.db).await?;

        Ok(())
    }

    async fn find_build_by_commit(
        &self,
        repo: &GithubRepoName,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>> {
        let build = build::Entity::find()
            .filter(
                build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(build::Column::CommitSha.eq(commit_sha.0)),
            )
            .one(&self.db)
            .await?;
        Ok(build.map(from_db_build))
    }
}

fn from_db_build(model: build::Model) -> BuildModel {
    BuildModel {
        id: model.id,
        repository: model.repository,
        commit_sha: model.commit_sha,
        created_at: from_db_datetime(model.created_at),
    }
}

fn from_db_datetime(datetime: NaiveDateTime) -> DateTime<Utc> {
    DateTime::from_utc(datetime, Utc)
}

fn full_repo_name(repo: &GithubRepoName) -> String {
    format!("{}/{}", repo.owner(), repo.name())
}
