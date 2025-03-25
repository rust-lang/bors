use sqlx::PgPool;

use crate::bors::{PullRequestStatus, RollupMode};
use crate::database::{
    BuildModel, BuildStatus, PullRequestModel, RepoModel, TreeState, WorkflowModel, WorkflowStatus,
    WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

use super::operations::{
    approve_pull_request, create_build, create_pull_request, create_workflow,
    delegate_pull_request, finalize_approval, find_build, find_pr_by_build,
    find_prs_pending_approval, find_prs_pending_approval_with_sha, get_pull_request,
    get_repository, get_running_builds, get_workflow_urls_for_build, get_workflows_for_build,
    remove_pending_approval, set_approval_pending, set_pr_priority, set_pr_rollup, set_pr_status,
    unapprove_pull_request, undelegate_pull_request, update_build_status,
    update_mergeable_states_by_base_branch, update_pr_build_id, update_workflow_status,
    upsert_pull_request, upsert_repository,
};
use super::{ApprovalInfo, MergeableState, RunId};

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
        pr: &PullRequestModel,
        approval_info: ApprovalInfo,
        priority: Option<u32>,
        rollup: Option<RollupMode>,
    ) -> anyhow::Result<()> {
        approve_pull_request(&self.pool, pr.id, approval_info, priority, rollup).await
    }

    pub async fn unapprove(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        unapprove_pull_request(&self.pool, pr.id).await
    }

    pub async fn set_priority(&self, pr: &PullRequestModel, priority: u32) -> anyhow::Result<()> {
        set_pr_priority(&self.pool, pr.id, priority).await
    }

    pub async fn delegate(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        delegate_pull_request(&self.pool, pr.id).await
    }

    pub async fn undelegate(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        undelegate_pull_request(&self.pool, pr.id).await
    }

    pub async fn update_mergeable_states_by_base_branch(
        &self,
        repo: &GithubRepoName,
        base_branch: &str,
        mergeable_state: MergeableState,
    ) -> anyhow::Result<u64> {
        update_mergeable_states_by_base_branch(&self.pool, repo, base_branch, mergeable_state).await
    }

    pub async fn set_rollup(
        &self,
        pr: &PullRequestModel,
        rollup: RollupMode,
    ) -> anyhow::Result<()> {
        set_pr_rollup(&self.pool, pr.id, rollup).await
    }

    pub async fn get_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        get_pull_request(&self.pool, repo, pr_number).await
    }

    pub async fn get_or_create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
        mergeable_state: MergeableState,
        pr_status: &PullRequestStatus,
    ) -> anyhow::Result<PullRequestModel> {
        let pr = upsert_pull_request(
            &self.pool,
            repo,
            pr_number,
            base_branch,
            mergeable_state,
            pr_status,
        )
        .await?;
        Ok(pr)
    }

    pub async fn create_pull_request(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        base_branch: &str,
        pr_status: PullRequestStatus,
    ) -> anyhow::Result<()> {
        create_pull_request(&self.pool, repo, pr_number, base_branch, pr_status).await
    }

    pub async fn set_pr_status(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        pr_status: PullRequestStatus,
    ) -> anyhow::Result<()> {
        set_pr_status(&self.pool, repo, pr_number, pr_status).await
    }

    pub async fn set_approval_pending(
        &self,
        pr: &PullRequestModel,
        approval_info: ApprovalInfo,
        priority: Option<u32>,
        rollup: Option<RollupMode>,
    ) -> anyhow::Result<()> {
        set_approval_pending(&self.pool, pr.id, approval_info, priority, rollup).await
    }

    pub async fn remove_pending_approval(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        remove_pending_approval(&self.pool, pr.id).await
    }

    pub async fn find_prs_pending_approval_with_sha(
        &self,
        repo: &GithubRepoName,
        commit_sha: &CommitSha,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        find_prs_pending_approval_with_sha(&self.pool, repo, commit_sha).await
    }

    pub async fn find_prs_pending_approval(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        find_prs_pending_approval(&self.pool, repo).await
    }

    pub async fn finalize_approval(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        finalize_approval(&self.pool, pr.id).await
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

    pub async fn repo_db(&self, repo: &GithubRepoName) -> anyhow::Result<Option<RepoModel>> {
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
