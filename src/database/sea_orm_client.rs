use anyhow::anyhow;
use axum::async_trait;
use chrono::{DateTime, NaiveDateTime, Utc};
use octocrab::models::RunId;
use sea_orm::sea_query::OnConflict;
use sea_orm::ActiveValue::{Set, Unchanged};
use sea_orm::{ActiveModelTrait, ColumnTrait, DbErr, EntityTrait, QueryFilter, TransactionTrait};

use entity::{build, pull_request, workflow};
use migration::sea_orm::DatabaseConnection;

use crate::database::{
    BuildModel, BuildStatus, DbClient, PullRequestModel, WorkflowModel, WorkflowStatus,
    WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

/// Provides access to a database using SeaORM mapping.
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
        parent: CommitSha,
    ) -> anyhow::Result<()> {
        let build = build::ActiveModel {
            repository: Set(pr.repository.clone()),
            branch: Set(branch),
            commit_sha: Set(commit_sha.0),
            parent: Set(parent.0),
            status: Set(build_status_to_db(BuildStatus::Pending).to_string()),
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

    async fn get_running_builds(&self, repo: &GithubRepoName) -> anyhow::Result<Vec<BuildModel>> {
        let builds = build::Entity::find()
            .filter(
                build::Column::Repository
                    .eq(full_repo_name(repo))
                    .and(build::Column::Status.eq(build_status_to_db(BuildStatus::Pending))),
            )
            .all(&self.db)
            .await?;
        Ok(builds.into_iter().map(build_from_db).collect())
    }

    async fn update_build_status(
        &self,
        build: &BuildModel,
        status: BuildStatus,
    ) -> anyhow::Result<()> {
        let model = build::ActiveModel {
            id: Unchanged(build.id),
            status: Set(build_status_to_db(status).to_string()),
            ..Default::default()
        };
        model.update(&self.db).await?;
        Ok(())
    }

    async fn create_workflow(
        &self,
        build: &BuildModel,
        name: String,
        url: String,
        run_id: RunId,
        workflow_type: WorkflowType,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        let model = workflow::ActiveModel {
            build: Set(build.id),
            name: Set(name),
            url: Set(url),
            run_id: Set(run_id.0 as i64),
            r#type: Set(workflow_type_to_db(workflow_type).to_string()),
            status: Set(workflow_status_to_db(&status).to_string()),
            ..Default::default()
        };
        model.insert(&self.db).await?;
        Ok(())
    }

    async fn update_workflow_status(
        &self,
        run_id: u64,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        let model = workflow::ActiveModel {
            status: Set(workflow_status_to_db(&status).to_string()),
            ..Default::default()
        };
        workflow::Entity::update_many()
            .set(model)
            .filter(workflow::Column::RunId.eq(run_id))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    async fn get_workflows_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<WorkflowModel>> {
        let workflows = workflow::Entity::find()
            .filter(workflow::Column::Build.eq(build.id))
            .find_also_related(build::Entity)
            .all(&self.db)
            .await?;
        Ok(workflows
            .into_iter()
            .map(|(workflow, build)| workflow_from_db(workflow, build))
            .collect())
    }
}

fn workflow_status_to_db(status: &WorkflowStatus) -> &'static str {
    match status {
        WorkflowStatus::Pending => "pending",
        WorkflowStatus::Success => "success",
        WorkflowStatus::Failure => "failure",
    }
}

fn workflow_status_from_db(status: String) -> WorkflowStatus {
    match status.as_str() {
        "pending" => WorkflowStatus::Pending,
        "success" => WorkflowStatus::Success,
        "failure" => WorkflowStatus::Failure,
        _ => {
            tracing::warn!("Encountered unknown workflow status in DB: {status}");
            WorkflowStatus::Pending
        }
    }
}

fn workflow_type_to_db(workflow_type: WorkflowType) -> &'static str {
    match workflow_type {
        WorkflowType::Github => "github",
        WorkflowType::External => "external",
    }
}

fn workflow_type_from_db(workflow_type: String) -> WorkflowType {
    match workflow_type.as_str() {
        "github" => WorkflowType::Github,
        "external" => WorkflowType::External,
        _ => panic!("Encountered unknown workflow type in DB: {workflow_type}"),
    }
}

fn workflow_from_db(workflow: workflow::Model, build: Option<build::Model>) -> WorkflowModel {
    WorkflowModel {
        id: workflow.id,
        build: build
            .map(build_from_db)
            .expect("Workflow without attached build"),
        name: workflow.name,
        url: workflow.url,
        run_id: RunId(workflow.run_id as u64),
        workflow_type: workflow_type_from_db(workflow.r#type),
        status: workflow_status_from_db(workflow.status),
        created_at: datetime_from_db(workflow.created_at),
    }
}

fn build_status_to_db(status: BuildStatus) -> &'static str {
    match status {
        BuildStatus::Pending => "pending",
        BuildStatus::Success => "success",
        BuildStatus::Failure => "failure",
        BuildStatus::Cancelled => "cancelled",
        BuildStatus::Timeouted => "timeouted",
    }
}

fn build_status_from_db(status: String) -> BuildStatus {
    match status.as_str() {
        "pending" => BuildStatus::Pending,
        "success" => BuildStatus::Success,
        "failure" => BuildStatus::Failure,
        "cancelled" => BuildStatus::Cancelled,
        "timeouted" => BuildStatus::Timeouted,
        _ => panic!("Encountered unknown build status in DB: {status}"),
    }
}

fn build_from_db(model: build::Model) -> BuildModel {
    BuildModel {
        id: model.id,
        repository: model.repository,
        branch: model.branch,
        commit_sha: model.commit_sha,
        parent: model.parent,
        status: build_status_from_db(model.status),
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
