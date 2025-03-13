use sqlx::PgPool;

use crate::bors::RollupMode;
use crate::database::{
    BuildModel, BuildStatus, PullRequestModel, RepoModel, TreeState, WorkflowModel, WorkflowStatus,
    WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

use super::operations::{
    approve_pull_request, create_build, create_pull_request, create_workflow,
    delegate_pull_request, find_build, find_pr_by_build, get_pull_request, get_repository,
    get_running_builds, get_workflow_urls_for_build, get_workflows_for_build, set_pr_priority,
    set_pr_rollup, unapprove_pull_request, undelegate_pull_request, update_build_status,
    update_pr_base_branch, update_pr_build_id, update_workflow_status, upsert_repository,
};
use super::{ApprovalInfo, RunId};

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
        approval_info: ApprovalInfo,
        priority: Option<u32>,
        base_branch: &str,
        rollup: Option<RollupMode>,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        approve_pull_request(&self.pool, pr.id, approval_info, priority, rollup).await
    }

    pub async fn unapprove(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        unapprove_pull_request(&self.pool, pr.id).await
    }

    pub async fn set_priority(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
        priority: u32,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        set_pr_priority(&self.pool, pr.id, priority).await
    }

    pub async fn delegate(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        delegate_pull_request(&self.pool, pr.id).await
    }

    pub async fn undelegate(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        undelegate_pull_request(&self.pool, pr.id).await
    }

    pub async fn set_rollup(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
        rollup: RollupMode,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        set_pr_rollup(&self.pool, pr.id, rollup).await
    }

    pub async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<PullRequestModel> {
        if let Some(pr) = get_pull_request(&self.pool, repo, pr_number).await? {
            return Ok(pr);
        }
        create_pull_request(&self.pool, repo, pr_number, base_branch).await?;
        let pr = get_pull_request(&self.pool, repo, pr_number)
            .await?
            .expect("PR not found after creation");

        Ok(pr)
    }

    pub async fn create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        create_pull_request(&self.pool, repo, pr_number, base_branch).await
    }

    pub async fn update_pr_base_branch(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
    ) -> anyhow::Result<()> {
        let pr = self
            .get_or_create_pull_request(repo, pr_number, base_branch)
            .await?;
        update_pr_base_branch(&self.pool, repo, pr.id, base_branch).await
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

    pub async fn get_workflow_urls_for_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Vec<String>> {
        get_workflow_urls_for_build(&self.pool, build.id).await
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

    pub async fn get_repository(&self, repo: &GithubRepoName) -> anyhow::Result<Option<RepoModel>> {
        get_repository(&self.pool, repo).await
    }

    pub async fn upsert_repository(
        &self,
        repo: &GithubRepoName,
        tree_state: TreeState,
    ) -> anyhow::Result<()> {
        upsert_repository(&self.pool, repo, tree_state).await
    }
}
