use sqlx::PgPool;

use crate::database::{
    BuildModel, BuildStatus, PullRequestModel, WorkflowModel, WorkflowStatus, WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

use super::operations::{
    approve_pull_request, create_build, create_pull_request, create_workflow, find_build,
    find_pr_by_build, get_pull_request, get_running_builds, get_workflows_for_build,
    set_pr_priority, unapprove_pull_request, update_build_status, update_pr_build_id,
    update_workflow_status,
};
use super::RunId;

/// Provides access to a database using sqlx operations.
#[derive(Clone)]
pub struct PgDbClient {
    pool: PgPool,
}

impl PgDbClient {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub async fn approve(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        approver: &str,
        priority: Option<u32>,
    ) -> anyhow::Result<()> {
        let pr = self.get_or_create_pull_request(repo, pr_number).await?;
        approve_pull_request(&self.pool, pr.id, approver, priority).await
    }

    pub async fn unapprove(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<()> {
        let pr = self.get_or_create_pull_request(repo, pr_number).await?;
        unapprove_pull_request(&self.pool, pr.id).await
    }

    pub async fn set_priority(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        priority: u32,
    ) -> anyhow::Result<()> {
        let pr = self.get_or_create_pull_request(repo, pr_number).await?;
        set_pr_priority(&self.pool, pr.id, priority).await
    }

    pub async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<PullRequestModel> {
        if let Some(pr) = get_pull_request(&self.pool, repo, pr_number).await? {
            return Ok(pr);
        }
        create_pull_request(&self.pool, repo, pr_number).await?;
        let pr = get_pull_request(&self.pool, repo, pr_number)
            .await?
            .expect("PR not found after creation");

        Ok(pr)
    }

    pub async fn find_pr_by_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        find_pr_by_build(&self.pool, build.id).await
    }

    pub async fn attach_try_build(
        &self,
        pr: PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
        parent: CommitSha,
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        let build_id =
            create_build(&mut *tx, &pr.repository, &branch, &commit_sha, &parent).await?;
        update_pr_build_id(&mut *tx, pr.id, build_id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn find_build(
        &self,
        repo: &GithubRepoName,
        branch: String,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>> {
        find_build(&self.pool, repo, &branch, &commit_sha).await
    }

    pub async fn get_running_builds(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<Vec<BuildModel>> {
        get_running_builds(&self.pool, repo).await
    }

    pub async fn update_build_status(
        &self,
        build: &BuildModel,
        status: BuildStatus,
    ) -> anyhow::Result<()> {
        update_build_status(&self.pool, build.id, status).await
    }

    pub async fn create_workflow(
        &self,
        build: &BuildModel,
        name: String,
        url: String,
        run_id: RunId,
        workflow_type: WorkflowType,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        create_workflow(
            &self.pool,
            build.id,
            &name,
            &url,
            run_id,
            workflow_type,
            status,
        )
        .await
    }

    pub async fn update_workflow_status(
        &self,
        run_id: u64,
        status: WorkflowStatus,
    ) -> anyhow::Result<()> {
        update_workflow_status(&self.pool, run_id, status).await
    }

    pub async fn get_workflows_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<WorkflowModel>> {
        get_workflows_for_build(&self.pool, build.id).await
    }

    pub async fn get_pending_workflows_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<RunId>> {
        let workflows = self
            .get_workflows_for_build(build)
            .await?
            .into_iter()
            .filter(|w| {
                w.status == WorkflowStatus::Pending && w.workflow_type == WorkflowType::Github
            })
            .map(|w| w.run_id)
            .collect::<Vec<_>>();
        Ok(workflows)
    }
}
