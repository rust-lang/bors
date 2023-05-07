use anyhow::anyhow;
use axum::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use sea_orm::sea_query::OnConflict;
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, TransactionTrait};

use entity::{build, pull_request, workflow};
use migration::sea_orm::DatabaseConnection;

use crate::database::{BuildModel, DbClient, PullRequestModel, WorkflowStatus};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

pub struct SeaORMClient {
    db: DatabaseConnection,
}

impl SeaORMClient {
    pub fn new(connection: DatabaseConnection) -> Self {
        Self { db: connection }
    }

    pub fn connection(&mut self) -> &mut DatabaseConnection {
        &mut self.db
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

        Ok(pr_from_db(pr, build))
    }

    async fn find_pr_by_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        let result = pull_request::Entity::find()
            .filter(pull_request::Column::TryBuild.eq(build.id))
            .find_also_related(build::Entity)
            .one(&self.db)
            .await?;
        Ok(result.map(|(pr, build)| pr_from_db(pr, build)))
    }

    async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<()> {
        let build = build::ActiveModel {
            repository: Set(pr.repository.clone()),
            branch: Set(branch),
            commit_sha: Set(commit_sha.0),
            ..Default::default()
        };

        let tx = self.db.begin().await?;
        let build = build::Entity::insert(build)
            .exec_with_returning(&tx)
            .await
            .map_err(|error| anyhow!("Cannot insert build into DB: {error:?}"))?;

        let pr_model = pull_request::ActiveModel {
            id: Unchanged(pr.id),
            try_build: Set(Some(build.id)),
            ..Default::default()
        };
        pr_model.update(&tx).await?;
        tx.commit().await?;

        Ok(())
    }

    async fn find_build(
        &self,
        repo: &GithubRepoName,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>> {
        let build = build::Entity::find()
            .filter(
                build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(build::Column::Branch.eq(branch))
                    .and(build::Column::CommitSha.eq(commit_sha.0)),
            )
            .one(&self.db)
            .await?;
        Ok(build.map(build_from_db))
    }

    async fn create_workflow(
        &self,
        build: &BuildModel,
        name: String,
        url: String,
        run_id: Option<u64>,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        let model = workflow::ActiveModel {
            build: Set(build.id),
            name: Set(name),
            url: Set(url),
            run_id: Set(run_id.map(|v| v as i64)),
            status: Set(workflow_status_to_db(&status).to_string()),
            ..Default::default()
        };
        model.insert(&self.db).await?;
        Ok(())
    }
}

fn workflow_status_to_db(status: &WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Success => "success",
        WorkflowStatus::Failure => "failure",
    }
}

fn build_from_db(model: build::Model) -> BuildModel {
    BuildModel {
        id: model.id,
        repository: model.repository,
        branch: model.branch,
        commit_sha: model.commit_sha,
        created_at: datetime_from_db(model.created_at),
    }
}

fn pr_from_db(pr: pull_request::Model, build: Option<build::Model>) -> PullRequestModel {
    PullRequestModel {
        id: pr.id,
        repository: pr.repository,
        number: PullRequestNumber(pr.number as u64),
        try_build: build.map(build_from_db),
        created_at: datetime_from_db(pr.created_at),
    }
}

fn datetime_from_db(datetime: NaiveDateTime) -> DateTime<Utc> {
    DateTime::from_utc(datetime, Utc)
}

fn full_repo_name(repo: &GithubRepoName) -> String {
    format!("{}/{}", repo.owner(), repo.name())
}
