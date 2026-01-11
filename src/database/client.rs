use sqlx::PgPool;

use crate::bors::comment::CommentTag;
use crate::bors::{BuildKind, PullRequestStatus, RollupMode};
use crate::database::operations::update_pr_auto_build_id;
use crate::database::{
    BuildModel, BuildStatus, CommentModel, PullRequestModel, RepoModel, TreeState, WorkflowModel,
    WorkflowStatus, WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};

use super::operations::{
    approve_pull_request, clear_auto_build, create_build, create_workflow, delegate_pull_request,
    delete_tagged_bot_comment, find_build, find_pr_by_build, get_nonclosed_pull_requests,
    get_pending_builds, get_prs_with_unknown_mergeability_or_approved, get_pull_request,
    get_repository, get_repository_by_name, get_tagged_bot_comments, get_workflow_urls_for_build,
    get_workflows_for_build, insert_repo_if_not_exists, record_tagged_bot_comment,
    set_pr_assignees, set_pr_mergeability_state, set_pr_priority, set_pr_rollup, set_pr_status,
    unapprove_pull_request, undelegate_pull_request, update_build_check_run_id,
    update_build_status, update_mergeable_states_by_base_branch, update_pr_try_build_id,
    update_workflow_status, upsert_pull_request, upsert_repository,
};
use super::{ApprovalInfo, DelegatedPermission, MergeableState, RunId, UpsertPullRequestParams};

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

    /// Unapprove a pull request and remove its auto build status, if there is any attached.
    pub async fn unapprove(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        unapprove_pull_request(&self.pool, pr.id).await
    }

    pub async fn clear_auto_build(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        clear_auto_build(&self.pool, pr.id).await
    }

    pub async fn set_priority(&self, pr: &PullRequestModel, priority: u32) -> anyhow::Result<()> {
        set_pr_priority(&self.pool, pr.id, priority).await
    }

    pub async fn delegate(
        &self,
        pr: &PullRequestModel,
        delegated_permission: DelegatedPermission,
    ) -> anyhow::Result<()> {
        delegate_pull_request(&self.pool, pr.id, delegated_permission).await
    }

    pub async fn undelegate(&self, pr: &PullRequestModel) -> anyhow::Result<()> {
        undelegate_pull_request(&self.pool, pr.id).await
    }

    pub async fn set_pr_mergeable_state(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        mergeable_state: MergeableState,
    ) -> anyhow::Result<()> {
        set_pr_mergeability_state(&self.pool, repo, pr_number, mergeable_state).await
    }

    pub async fn set_pr_assignees(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        assignees: &[String],
    ) -> anyhow::Result<()> {
        set_pr_assignees(&self.pool, repo, pr_number, assignees).await
    }

    /// Sets the mergeability status of all PRs that were not marked as unmergeable before
    /// with the given `base_branch` to `mergeability_state`.
    /// Returns the list of pull requests that target this base branch.
    /// Their state will be set to BEFORE the mergeability change was made.
    pub async fn update_mergeable_states_by_base_branch(
        &self,
        repo: &GithubRepoName,
        base_branch: &str,
        mergeability_state: MergeableState,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        update_mergeable_states_by_base_branch(&self.pool, repo, base_branch, mergeability_state)
            .await
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

    pub async fn upsert_pull_request(
        &self,
        repo: &GithubRepoName,
        params: UpsertPullRequestParams,
    ) -> anyhow::Result<PullRequestModel> {
        let pr = upsert_pull_request(&self.pool, repo, &params).await?;
        Ok(pr)
    }

    pub async fn get_prs_with_unknown_mergeability_or_approved(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        get_prs_with_unknown_mergeability_or_approved(&self.pool, repo).await
    }

    pub async fn get_nonclosed_pull_requests(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        get_nonclosed_pull_requests(&self.pool, repo).await
    }

    pub async fn set_pr_status(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        pr_status: PullRequestStatus,
    ) -> anyhow::Result<()> {
        set_pr_status(&self.pool, repo, pr_number, pr_status).await
    }

    pub async fn find_pr_by_build(
        &self,
        build: &BuildModel,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        find_pr_by_build(&self.pool, build.id).await
    }

    pub async fn attach_try_build(
        &self,
        pr: &PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
        parent: CommitSha,
    ) -> anyhow::Result<i32> {
        let mut tx = self.pool.begin().await?;
        let build_id = create_build(
            &mut *tx,
            &pr.repository,
            &branch,
            BuildKind::Try,
            &commit_sha,
            &parent,
        )
        .await?;
        update_pr_try_build_id(&mut *tx, pr.id, build_id).await?;
        tx.commit().await?;
        Ok(build_id)
    }

    pub async fn attach_auto_build(
        &self,
        pr: &PullRequestModel,
        branch: String,
        commit_sha: CommitSha,
        parent: CommitSha,
    ) -> anyhow::Result<i32> {
        let mut tx = self.pool.begin().await?;
        let build_id = create_build(
            &mut *tx,
            &pr.repository,
            &branch,
            BuildKind::Auto,
            &commit_sha,
            &parent,
        )
        .await?;
        update_pr_auto_build_id(&mut *tx, pr.id, build_id).await?;
        tx.commit().await?;
        Ok(build_id)
    }

    pub async fn find_build(
        &self,
        repo: &GithubRepoName,
        branch: &str,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Option<BuildModel>> {
        find_build(&self.pool, repo, branch, &commit_sha).await
    }

    pub async fn get_pending_builds(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<Vec<BuildModel>> {
        get_pending_builds(&self.pool, repo).await
    }

    pub async fn update_build_status(
        &self,
        build: &BuildModel,
        status: BuildStatus,
    ) -> anyhow::Result<()> {
        update_build_status(&self.pool, build.id, status).await
    }

    pub async fn update_build_check_run_id(
        &self,
        build_id: i32,
        check_run_id: i64,
    ) -> anyhow::Result<()> {
        update_build_check_run_id(&self.pool, build_id, check_run_id).await
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
    ) -> anyhow::Result<Vec<WorkflowModel>> {
        let workflows = self
            .get_workflows_for_build(build)
            .await?
            .into_iter()
            .filter(|w| {
                w.status == WorkflowStatus::Pending && w.workflow_type == WorkflowType::Github
            })
            .collect::<Vec<WorkflowModel>>();
        Ok(workflows)
    }

    pub async fn repo_db(&self, repo: &GithubRepoName) -> anyhow::Result<Option<RepoModel>> {
        get_repository(&self.pool, repo).await
    }

    pub async fn insert_repo_if_not_exists(
        &self,
        repo: &GithubRepoName,
        tree_state: TreeState,
    ) -> anyhow::Result<()> {
        insert_repo_if_not_exists(&self.pool, repo, tree_state).await
    }

    pub async fn repo_by_name(&self, repo_name: &str) -> anyhow::Result<Option<RepoModel>> {
        get_repository_by_name(&self.pool, repo_name).await
    }

    pub async fn upsert_repository(
        &self,
        repo: &GithubRepoName,
        tree_state: TreeState,
    ) -> anyhow::Result<()> {
        upsert_repository(&self.pool, repo, tree_state).await
    }

    pub async fn get_tagged_bot_comments(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        label: CommentTag,
    ) -> anyhow::Result<Vec<CommentModel>> {
        get_tagged_bot_comments(&self.pool, repo, pr_number, label).await
    }

    pub async fn record_tagged_bot_comment(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        label: CommentTag,
        node_id: &str,
    ) -> anyhow::Result<()> {
        record_tagged_bot_comment(&self.pool, repo, pr_number, label, node_id).await
    }

    pub async fn delete_tagged_bot_comment(&self, comment: &CommentModel) -> anyhow::Result<()> {
        delete_tagged_bot_comment(&self.pool, comment.id).await
    }
}
