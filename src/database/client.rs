use super::operations::{
    approve_pull_request, clear_auto_build, create_build, create_workflow, delegate_pull_request,
    delete_tagged_bot_comment, find_build, find_pr_by_build, get_nonclosed_pull_requests,
    get_pending_builds, get_prs_with_stale_mergeability_or_approved, get_pull_request,
    get_repository, get_repository_by_name, get_tagged_bot_comments, get_workflow_urls_for_build,
    get_workflows_for_build, insert_repo_if_not_exists, record_tagged_bot_comment,
    set_pr_assignees, set_pr_mergeability_state, set_pr_priority, set_pr_rollup, set_pr_status,
    set_stale_mergeability_status_by_base_branch, unapprove_pull_request, undelegate_pull_request,
    update_build, update_pr_try_build_id, update_workflow_status, upsert_pull_request,
    upsert_repository,
};
use super::{
    ApprovalInfo, DelegatedPermission, MergeableState, PrimaryKey, RunId, UpdateBuildParams,
    UpsertPullRequestParams,
};
use std::collections::{HashMap, HashSet};

use crate::bors::comment::CommentTag;
use crate::bors::{BuildKind, PullRequestStatus, RollupMode};
use crate::database::operations::{
    get_nonclosed_rollups, register_rollup_pr_member, update_pr_auto_build_id,
};
use crate::database::{
    BuildModel, CommentModel, PullRequestModel, RepoModel, TreeState, WorkflowModel,
    WorkflowStatus, WorkflowType,
};
use crate::github::PullRequestNumber;
use crate::github::{CommitSha, GithubRepoName};
use anyhow::Context;
use itertools::Either;
use sqlx::PgPool;
use sqlx::postgres::PgAdvisoryLock;
use tracing::log;

/// Provides access to a database using sqlx operations.
#[derive(Clone)]
pub struct PgDbClient {
    pub(super) pool: PgPool,
}

impl PgDbClient {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Tries to perform the given asynchronous operation if there is no other concurrent operation
    /// operation on the same `lock_name` at the same time.
    ///
    /// **If it is not possible to take the lock, then `func` will NOT be called at all!**
    pub async fn ensure_not_concurrent<Func, R>(
        &self,
        lock_name: &str,
        func: Func,
    ) -> anyhow::Result<ExclusiveOperationOutcome<R>>
    where
        Func: AsyncFnOnce(ExclusiveLockProof) -> R,
    {
        let lock = PgAdvisoryLock::new(lock_name);

        // Try to acquire the lock
        let _guard = match lock
            .try_acquire(self.pool.acquire().await?)
            .await
            .with_context(|| anyhow::anyhow!("Cannot acquire advisory lock {lock_name}"))?
        {
            Either::Left(guard) => guard,
            Either::Right(_conn) => {
                // Something is doing the same operation concurrently. Immediately return.
                return Ok(ExclusiveOperationOutcome::Skipped);
            }
        };
        // Create lock proof
        let proof = ExclusiveLockProof { _proof: () };
        // Run the "atomic" operation
        let res = func(proof).await;
        // Try to unlock the lock explicitly, to avoid waiting for the next DB operation on this
        // connection.
        if let Err(error) = _guard.release_now().await {
            log::error!("Cannot unlock advisory lock {lock_name}: {error:?}");
        }
        Ok(ExclusiveOperationOutcome::Performed(res))
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

    /// Set the mergeability status of a PR, and return the *previous* mergeability status.
    /// Also clears the `mergeable_is_stale` flag on the PR.
    pub async fn set_pr_mergeable_state(
        &self,
        repo: &GithubRepoName,
        pr_number: PullRequestNumber,
        mergeable_state: MergeableState,
    ) -> anyhow::Result<MergeableState> {
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

    /// Sets the `mergeable_is_stale` flag with the given `base_branch`.
    /// Returns the list of pull requests that target this base branch.
    pub async fn set_stale_mergeability_status_by_base_branch(
        &self,
        repo: &GithubRepoName,
        base_branch: &str,
    ) -> anyhow::Result<Vec<PullRequestModel>> {
        set_stale_mergeability_status_by_base_branch(&self.pool, repo, base_branch).await
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

    /// Create or update a pull request in the database.
    /// Returns the updated PR state from the database.
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
        get_prs_with_stale_mergeability_or_approved(&self.pool, repo).await
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

    pub async fn update_build(
        &self,
        build_id: PrimaryKey,
        params: UpdateBuildParams,
    ) -> anyhow::Result<()> {
        update_build(&self.pool, build_id, params).await
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

    /// Register the members of a rollup.
    /// All records are inserted in a single transaction.
    pub async fn register_rollup_members(
        &self,
        rollup: &PullRequestModel,
        members: &[PullRequestModel],
    ) -> anyhow::Result<()> {
        let mut tx = self.pool.begin().await?;
        for member in members {
            assert_ne!(rollup.id, member.id);
            assert_eq!(rollup.repository, member.repository);
            register_rollup_pr_member(&mut *tx, rollup, member).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Returns a map of rollup PR numbers to the set of member PR numbers that are part of that rollup.
    /// Only returns non-closed rollup PRs.
    pub async fn get_nonclosed_rollups(
        &self,
        repo: &GithubRepoName,
    ) -> anyhow::Result<HashMap<PullRequestNumber, HashSet<PullRequestNumber>>> {
        get_nonclosed_rollups(&self.pool, repo).await
    }
}

pub enum ExclusiveOperationOutcome<R> {
    /// The operation was performed.
    Performed(R),
    /// There was concurrent interference, the operation was not performed.
    Skipped,
}

/// Proof that a Postgres session advisory lock is held.
/// This should be used when perform GitHub API operations related to merges that have to
/// be atomic and shouldn't be performed by multiple concurrently running bors instances (this can
/// happen during redeploys).
pub struct ExclusiveLockProof {
    _proof: (),
}
