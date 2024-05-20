use axum::async_trait;
use sqlx::PgPool;

use crate::database::operations::get_all_workflows;
use crate::{
    database::{
        BuildModel, BuildStatus, DbClient, PullRequestModel, RunId, WorkflowModel, WorkflowStatus,
        WorkflowType,
    },
    github::{CommitSha, GithubRepoName, PullRequestNumber},
    PgDbClient,
};

pub(crate) struct MockedDBClient {
    pool: PgPool,
    db: PgDbClient,
    pub get_workflows_for_build:
        Option<Box<dyn Fn() -> anyhow::Result<Vec<WorkflowModel>> + Send + Sync>>,
}

impl MockedDBClient {
    pub(crate) fn new(pool: PgPool) -> Self {
        let db = PgDbClient::new(pool.clone());
        Self {
            db,
            pool,
            get_workflows_for_build: None,
        }
    }

    pub(crate) async fn get_all_workflows(&self) -> anyhow::Result<Vec<WorkflowModel>> {
        get_all_workflows(&self.pool).await
    }
}

#[async_trait]
impl DbClient for MockedDBClient {
    async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<PullRequestModel> {
        self.db.get_or_create_pull_request(repo, pr_number).await
    }

    async fn find_pr_by_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        self.db.find_pr_by_build(build).await
    }

    async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
        parent: CommitSha,
    ) -> anyhow::Result<()> {
        self.db
            .attach_try_build(pr, branch, commit_sha, parent)
            .await
    }

    async fn find_build(
        &self,
        repo: &GithubRepoName,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>> {
        self.db.find_build(repo, branch, commit_sha).await
    }

    async fn get_running_builds(&self, repo: &GithubRepoName) -> anyhow::Result<Vec<BuildModel>> {
        self.db.get_running_builds(repo).await
    }

    async fn update_build_status(
        &self,
        build: &BuildModel,
        status: BuildStatus,
    ) -> anyhow::Result<()> {
        self.db.update_build_status(build, status).await
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
        self.db
            .create_workflow(build, name, url, run_id, workflow_type, status)
            .await
    }

    /// Updates the status of a workflow with the given run ID in the DB.
    async fn update_workflow_status(
        &self,
        run_id: u64,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        self.db.update_workflow_status(run_id, status).await
    }

    /// Get all workflows attached to a build.
    async fn get_workflows_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<WorkflowModel>> {
        if let Some(f) = &self.get_workflows_for_build {
            f()
        } else {
            self.db.get_workflows_for_build(build).await
        }
    }
}
