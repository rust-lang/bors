use anyhow::anyhow;
use octocrab::models::CheckRunId;
use octocrab::params::checks::{CheckRunConclusion, CheckRunOutput, CheckRunStatus};
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BorsContext;
use crate::bors::comment::{
    auto_build_push_failed_comment, auto_build_started_comment, auto_build_succeeded_comment,
};
use crate::bors::{PullRequestStatus, RepositoryState};
use crate::database::{BuildStatus, PullRequestModel};
use crate::github::api::client::GithubRepositoryClient;
use crate::github::api::operations::ForcePush;
use crate::github::{CommitSha, MergeError};
use crate::utils::sort_queue::sort_queue_prs;

enum MergeResult {
    Success(CommitSha),
    Conflict,
}

#[derive(Debug)]
enum MergeQueueEvent {
    Trigger,
    Shutdown,
}

#[derive(Clone)]
pub struct MergeQueueSender {
    inner: mpsc::Sender<MergeQueueEvent>,
}

impl MergeQueueSender {
    pub async fn trigger(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(MergeQueueEvent::Trigger)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub fn shutdown(&self) {
        let _ = self.inner.try_send(MergeQueueEvent::Shutdown);
    }
}

/// Branch used for performing merge operations.
/// This branch should not run CI checks.
pub(super) const AUTO_MERGE_BRANCH_NAME: &str = "automation/bors/auto-merge";

/// Branch where CI checks run for auto builds.
/// This branch should run CI checks.
pub(super) const AUTO_BRANCH_NAME: &str = "automation/bors/auto";

// The name of the check run seen in the GitHub UI.
pub(super) const AUTO_BUILD_CHECK_RUN_NAME: &str = "Bors auto build";

pub async fn merge_queue_tick(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();

    for repo in repos {
        let repo_name = repo.repository();
        let repo_db = match ctx.db.repo_db(repo_name).await? {
            Some(repo) => repo,
            None => {
                tracing::error!("Repository {repo_name} not found");
                continue;
            }
        };

        if !repo.config.load().merge_queue_enabled {
            continue;
        }

        let priority = repo_db.tree_state.priority();
        let prs = ctx.db.get_merge_queue_prs(repo_name, priority).await?;

        // Sort PRs according to merge queue priority rules.
        // Successful builds come first so they can be merged immediately,
        // then pending builds (which block the queue to prevent starting simultaneous auto-builds).
        let prs = sort_queue_prs(prs);

        for pr in prs {
            let pr_num = pr.number;

            if let Some(auto_build) = &pr.auto_build {
                let commit_sha = CommitSha(auto_build.commit_sha.clone());

                match auto_build.status {
                    // Build successful - point the base branch to the merged commit.
                    BuildStatus::Success => {
                        let workflows = ctx.db.get_workflows_for_build(auto_build).await?;
                        let comment = auto_build_succeeded_comment(
                            &workflows,
                            pr.approver().unwrap_or("<unknown>"),
                            &commit_sha,
                            &pr.base_branch,
                        );
                        repo.client.post_comment(pr.number, comment).await?;

                        match repo
                            .client
                            .set_branch_to_sha(&pr.base_branch, &commit_sha, ForcePush::No)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!("Auto build succeeded and merged for PR {pr_num}");

                                ctx.db
                                    .set_pr_status(
                                        &pr.repository,
                                        pr.number,
                                        PullRequestStatus::Merged,
                                    )
                                    .await?;
                            }
                            Err(error) => {
                                tracing::error!(
                                    "Failed to push PR {pr_num} to base branch: {:?}",
                                    error
                                );

                                if let Some(check_run_id) = auto_build.check_run_id {
                                    if let Err(error) = repo
                                        .client
                                        .update_check_run(
                                            CheckRunId(check_run_id as u64),
                                            CheckRunStatus::Completed,
                                            Some(CheckRunConclusion::Failure),
                                            None,
                                        )
                                        .await
                                    {
                                        tracing::error!(
                                            "Could not update check run {check_run_id}: {error:?}"
                                        );
                                    }
                                }

                                ctx.db
                                    .update_build_status(auto_build, BuildStatus::Failure)
                                    .await?;

                                let comment = auto_build_push_failed_comment(&error.to_string());
                                repo.client.post_comment(pr.number, comment).await?;
                            }
                        };

                        // Break to give GitHub time to update the base branch.
                        break;
                    }
                    // Build in progress - stop queue. We can only have one PR being built
                    // at a time.
                    BuildStatus::Pending => {
                        tracing::info!("PR {pr_num} has a pending build - blocking queue");
                        break;
                    }
                    BuildStatus::Failure | BuildStatus::Cancelled | BuildStatus::Timeouted => {
                        unreachable!("Failed auto builds should be filtered out by SQL query");
                    }
                }
            }

            // No build exists for this PR - start a new auto build.
            match start_auto_build(&repo, &ctx, pr).await {
                Ok(true) => {
                    tracing::info!("Starting auto build for PR {pr_num}");
                    break;
                }
                Ok(false) => {
                    tracing::debug!("Failed to start auto build for PR {pr_num}");
                    continue;
                }
                Err(error) => {
                    // Unexpected error - the PR will remain in the "queue" for a retry.
                    tracing::error!("Error starting auto build for PR {pr_num}: {:?}", error);
                    continue;
                }
            }
        }
    }

    #[cfg(test)]
    crate::bors::WAIT_FOR_MERGE_QUEUE.mark();

    Ok(())
}

/// Starts a new auto build for a pull request.
async fn start_auto_build(
    repo: &Arc<RepositoryState>,
    ctx: &Arc<BorsContext>,
    pr: PullRequestModel,
) -> anyhow::Result<bool> {
    let client = &repo.client;

    let gh_pr = client.get_pull_request(pr.number).await?;
    let base_sha = client.get_branch_sha(&pr.base_branch).await?;

    let auto_merge_commit_message = format!(
        "Auto merge of #{} - {}, r={}\n\n{}\n\n{}",
        pr.number,
        gh_pr.head_label,
        pr.approver().unwrap_or("<unknown>"),
        pr.title,
        gh_pr.message
    );

    // 1. Merge PR head with base branch on `AUTO_MERGE_BRANCH_NAME`
    match attempt_merge(
        &repo.client,
        &gh_pr.head.sha,
        &base_sha,
        &auto_merge_commit_message,
    )
    .await?
    {
        MergeResult::Success(merge_sha) => {
            // 2. Push merge commit to `AUTO_BRANCH_NAME` where CI runs
            client
                .set_branch_to_sha(AUTO_BRANCH_NAME, &merge_sha, ForcePush::Yes)
                .await?;

            // 3. Record the build in the database
            let build_id = ctx
                .db
                .attach_auto_build(
                    &pr,
                    AUTO_BRANCH_NAME.to_string(),
                    merge_sha.clone(),
                    base_sha,
                )
                .await?;

            // 4. Set GitHub check run to pending on PR head
            match client
                .create_check_run(
                    AUTO_BUILD_CHECK_RUN_NAME,
                    &gh_pr.head.sha,
                    CheckRunStatus::InProgress,
                    CheckRunOutput {
                        title: AUTO_BUILD_CHECK_RUN_NAME.to_string(),
                        summary: "".to_string(),
                        text: None,
                        annotations: vec![],
                        images: vec![],
                    },
                    &build_id.to_string(),
                )
                .await
            {
                Ok(check_run) => {
                    tracing::info!(
                        "Created check run {} for build {build_id}",
                        check_run.id.into_inner()
                    );
                    ctx.db
                        .update_build_check_run_id(build_id, check_run.id.into_inner() as i64)
                        .await?;
                }
                Err(error) => {
                    // Check runs aren't critical, don't block progress if they fail
                    tracing::error!("Cannot create check run: {error:?}");
                }
            }

            // 5. Post status comment
            let comment = auto_build_started_comment(&gh_pr.head.sha, &merge_sha);
            client.post_comment(pr.number, comment).await?;

            Ok(true)
        }
        MergeResult::Conflict => Ok(false),
    }
}

/// Attempts to merge the given head SHA with base SHA via `AUTO_MERGE_BRANCH_NAME`.
async fn attempt_merge(
    client: &GithubRepositoryClient,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
    merge_message: &str,
) -> anyhow::Result<MergeResult> {
    tracing::debug!("Attempting to merge with base SHA {base_sha}");

    // Reset auto merge branch to point to base branch
    client
        .set_branch_to_sha(AUTO_MERGE_BRANCH_NAME, base_sha, ForcePush::Yes)
        .await
        .map_err(|error| anyhow!("Cannot set auto merge branch to {}: {error:?}", base_sha.0))?;

    // then merge PR head commit into auto merge branch.
    match client
        .merge_branches(AUTO_MERGE_BRANCH_NAME, head_sha, merge_message)
        .await
    {
        Ok(merge_sha) => {
            tracing::debug!("Merge successful, SHA: {merge_sha}");
            Ok(MergeResult::Success(merge_sha))
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");
            Ok(MergeResult::Conflict)
        }
        Err(error) => Err(error.into()),
    }
}

pub fn start_merge_queue(ctx: Arc<BorsContext>) -> (MergeQueueSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<MergeQueueEvent>(10);
    let sender = MergeQueueSender { inner: tx };

    let fut = async move {
        while let Some(event) = rx.recv().await {
            match event {
                MergeQueueEvent::Trigger => {
                    let span = tracing::info_span!("MergeQueue");
                    tracing::debug!("Processing merge queue");
                    if let Err(error) = merge_queue_tick(ctx.clone()).instrument(span.clone()).await
                    {
                        // In tests, we want to panic on all errors.
                        #[cfg(test)]
                        {
                            panic!("Merge queue handler failed: {error:?}");
                        }
                        #[cfg(not(test))]
                        {
                            use crate::utils::logging::LogError;
                            span.log_error(error);
                        }
                    }
                }
                MergeQueueEvent::Shutdown => {
                    tracing::debug!("Merge queue received shutdown signal");
                    break;
                }
            }
        }
    };

    (sender, fut)
}

#[cfg(test)]
mod tests {

    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
    use sqlx::PgPool;

    use crate::{
        bors::{
            PullRequestStatus,
            merge_queue::{AUTO_BRANCH_NAME, AUTO_BUILD_CHECK_RUN_NAME, AUTO_MERGE_BRANCH_NAME},
        },
        database::{BuildStatus, WorkflowStatus, operations::get_all_workflows},
        github::CommitSha,
        tests::{BorsBuilder, BorsTester, Comment, GitHubState, WorkflowEvent, default_repo_name},
    };

    fn gh_state_with_merge_queue() -> GitHubState {
        GitHubState::default().with_default_config(
            r#"
      merge_queue_enabled = true
      "#,
        )
    }

    pub async fn run_merge_queue_test<F: AsyncFnOnce(&mut BorsTester) -> anyhow::Result<()>>(
        pool: PgPool,
        f: F,
    ) -> GitHubState {
        BorsBuilder::new(pool)
            .github(gh_state_with_merge_queue())
            .run_test(f)
            .await
    }

    async fn start_auto_build(tester: &mut BorsTester) -> anyhow::Result<()> {
        tester.post_comment("@bors r+").await?;
        tester.expect_comments(1).await;
        tester.process_merge_queue().await;
        tester.expect_comments(1).await;
        Ok(())
    }

    #[sqlx::test]
    async fn auto_workflow_started(pool: sqlx::PgPool) {
        run_merge_queue_test(pool.clone(), async |tester| {
            start_auto_build(tester).await?;
            tester
                .workflow_event(WorkflowEvent::started(tester.auto_branch()))
                .await?;
            Ok(())
        })
        .await;

        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn auto_workflow_check_run_created(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.expect_check_run(
                &tester.default_pr().await.get_gh_pr().head_sha,
                AUTO_BUILD_CHECK_RUN_NAME,
                AUTO_BUILD_CHECK_RUN_NAME,
                CheckRunStatus::InProgress,
                None,
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_started_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-0-pr-1..."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_insert_into_db(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;
            assert!(
                tester
                    .db()
                    .find_build(
                        &default_repo_name(),
                        AUTO_BRANCH_NAME.to_string(),
                        CommitSha(tester.auto_branch().get_sha().to_string()),
                    )
                    .await?
                    .is_some()
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            :sunny: Test successful - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)
            Approved by: `default-user`
            Pushing merge-0-pr-1 to `main`...
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_succeeds_and_merges_in_db(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;
            tester
                .wait_for_default_pr(|pr| {
                    pr.auto_build.as_ref().unwrap().status == BuildStatus::Success
                })
                .await?;
            tester
                .wait_for_default_pr(|pr| pr.pr_status == PullRequestStatus::Merged)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_fail_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;

            tester.default_repo().lock().push_error = true;

            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @":eyes: Test was successful, but fast-forwarding failed: IO error"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_fail_updates_check_run(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;

            tester.default_repo().lock().push_error = true;

            tester.process_merge_queue().await;
            tester.expect_comments(1).await;
            tester.expect_check_run(
                &tester.default_pr().await.get_gh_pr().head_sha,
                AUTO_BUILD_CHECK_RUN_NAME,
                AUTO_BUILD_CHECK_RUN_NAME,
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Failure),
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_fail_in_db(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;

            tester.default_repo().lock().push_error = true;

            tester.process_merge_queue().await;
            tester.expect_comments(1).await;
            tester
                .wait_for_default_pr(|pr| {
                    pr.auto_build.as_ref().unwrap().status == BuildStatus::Failure
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_branch_history(pool: sqlx::PgPool) {
        let gh = run_merge_queue_test(pool, async |tester| {
            start_auto_build(tester).await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;
            Ok(())
        })
        .await;
        gh.check_sha_history(default_repo_name(), "main", &["main-sha1", "merge-0-pr-1"]);
        gh.check_sha_history(
            default_repo_name(),
            AUTO_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-0-pr-1"],
        );
        gh.check_sha_history(default_repo_name(), AUTO_BRANCH_NAME, &["merge-0-pr-1"]);
    }

    #[sqlx::test]
    async fn merge_queue_sequential_order(pool: sqlx::PgPool) {
        let gh = run_merge_queue_test(pool, async |tester| {
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = tester.open_pr(default_repo_name(), |_| {}).await?;

            tester.post_comment("@bors r+").await?;
            tester
                .post_comment(Comment::pr(pr2.number.0, "@bors r+"))
                .await?;
            tester
                .post_comment(Comment::pr(pr3.number.0, "@bors r+"))
                .await?;

            tester.expect_comments(1).await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;

            tester.process_merge_queue().await;
            tester.expect_comments(1).await;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;

            tester.process_merge_queue().await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;

            tester.process_merge_queue().await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;

            Ok(())
        })
        .await;

        gh.check_sha_history(
            default_repo_name(),
            "main",
            &["main-sha1", "merge-0-pr-1", "merge-1-pr-2", "merge-2-pr-3"],
        );
    }

    #[sqlx::test]
    async fn merge_queue_priority_order(pool: sqlx::PgPool) {
        let gh = run_merge_queue_test(pool, async |tester| {
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = tester.open_pr(default_repo_name(), |_| {}).await?;

            tester.post_comment("@bors r+").await?;
            tester
                .post_comment(Comment::pr(pr2.number.0, "@bors r+"))
                .await?;
            tester
                .post_comment(Comment::pr(pr3.number.0, "@bors r+ p=3"))
                .await?;

            tester.expect_comments(1).await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;

            tester.process_merge_queue().await;
            tester.expect_comments(1).await;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester.expect_comments(1).await;

            tester.process_merge_queue().await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr3.number.0)
                .await?;

            tester.process_merge_queue().await;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;
            tester.workflow_full_success(tester.auto_branch()).await?;
            tester
                .expect_comment_on_pr(default_repo_name(), pr2.number.0)
                .await?;

            Ok(())
        })
        .await;

        gh.check_sha_history(
            default_repo_name(),
            "main",
            &["main-sha1", "merge-0-pr-1", "merge-1-pr-3", "merge-2-pr-2"],
        );
    }
}
