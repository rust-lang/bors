use chrono::{DateTime, Utc};
use octocrab::models::checks::CheckRun;
use octocrab::params::checks::CheckRunStatus;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BorsContext;
use crate::bors::comment::{
    auto_build_push_failed_comment, auto_build_started_comment, auto_build_succeeded_comment,
    merge_conflict_comment,
};
use crate::bors::{PullRequestStatus, RepositoryState};
use crate::database::{BuildStatus, MergeableState, OctocrabMergeableState, PullRequestModel};
use crate::github::api::client::CheckRunOutput;
use crate::github::api::operations::{BranchUpdateError, ForcePush};
use crate::github::{CommitSha, PullRequest};
use crate::github::{MergeResult, attempt_merge};
use crate::utils::sort_queue::sort_queue_prs;

use super::{MergeType, create_merge_commit_message};

#[derive(Debug)]
enum MergeQueueEvent {
    /// Directly run the merge queue.
    /// Should only be used in tests.
    #[cfg(test)]
    PerformTick,
    /// Check if enough time has passed (or notify was called) for the merge queue to run.
    /// If yes, performs a single merge queue tick.
    MaybePerformTick,
    /// Tells the merge queue that some interesting event has happened and it should thus run
    /// soon(er).
    Notify,
    /// Shutdown the merge queue.
    Shutdown,
}

#[derive(Clone)]
pub struct MergeQueueSender {
    inner: mpsc::Sender<MergeQueueEvent>,
}

impl MergeQueueSender {
    /// Run the merge queue.
    /// Only allowed in tests.
    #[cfg(test)]
    pub async fn perform_tick(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(MergeQueueEvent::PerformTick)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    /// Try to run the merge queue, if a notify has happened since the last tick, or if enough time
    /// has passed.
    pub async fn maybe_perform_tick(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(MergeQueueEvent::MaybePerformTick)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    /// Tells the merge queue that some interesting event has happened, and it should thus run
    /// sooner than.
    pub async fn notify(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(MergeQueueEvent::Notify)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    /// Shutdown the merge queue.
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

/// Process the merge queue.
/// Try to finish and merge a successful auto build, if any.
/// If there is a PR ready to be merged, starts an auto build for it.
pub async fn merge_queue_tick(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();

    for repo in repos {
        let repo_name = repo.repository().to_string();
        if let Err(error) = process_repository(&repo, &ctx).await {
            tracing::error!("Error running merge queue for {repo_name}: {error:?}");
        }
    }

    Ok(())
}

async fn process_repository(repo: &RepositoryState, ctx: &BorsContext) -> anyhow::Result<()> {
    if !repo.config.load().merge_queue_enabled {
        return Ok(());
    }

    let repo_name = repo.repository();
    let repo_db = match ctx.db.repo_db(repo_name).await? {
        Some(repo) => repo,
        None => {
            tracing::error!("Repository {repo_name} not found");
            return Ok(());
        }
    };

    let priority = repo_db.tree_state.priority();
    let prs = ctx.db.get_merge_queue_prs(repo_name, priority).await?;

    // Sort PRs according to merge queue priority rules.
    // Successful builds come first so they can be merged immediately,
    // then pending builds (which block the queue to prevent starting simultaneous auto-builds).
    let prs = sort_queue_prs(prs);

    for pr in prs {
        let pr_num = pr.number;

        let Some(auto_build) = &pr.auto_build else {
            // No build exists for this PR - start a new auto build.
            match start_auto_build(repo, ctx, &pr).await {
                Ok(()) => {
                    tracing::info!("Starting auto build for PR {pr_num}");
                    break;
                }
                Err(StartAutoBuildError::MergeConflict) => {
                    let gh_pr = repo.client.get_pull_request(pr.number).await?;

                    tracing::debug!(
                        "Failed to start auto build for PR {pr_num} due to merge conflict"
                    );

                    ctx.db
                        .update_pr_mergeable_state(&pr, MergeableState::Unknown)
                        .await?;
                    repo.client
                        .post_comment(pr.number, merge_conflict_comment(&gh_pr.head.name))
                        .await?;
                    continue;
                }
                Err(StartAutoBuildError::SanityCheckFailed(error)) => {
                    tracing::info!("Sanity check failed for PR {pr_num}: {error:?}");
                    break;
                }
                Err(StartAutoBuildError::GitHubError(error)) => {
                    tracing::debug!(
                        "Failed to start auto build for PR {pr_num} due to a GitHub error: {error:?}"
                    );
                    break;
                }
                Err(StartAutoBuildError::DatabaseError(error)) => {
                    tracing::debug!(
                        "Failed to start auto build for PR {pr_num} due to database error: {error:?}"
                    );
                    break;
                }
            }
        };

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

                if let Err(error) = repo
                    .client
                    .set_branch_to_sha(&pr.base_branch, &commit_sha, ForcePush::No)
                    .await
                {
                    tracing::error!(
                        "Failed to fast-forward base branch for PR {pr_num}: {error:?}"
                    );

                    let comment = match &error {
                        BranchUpdateError::Conflict(branch_name) => auto_build_push_failed_comment(
                            &format!("this PR has conflicts with the `{branch_name}` branch"),
                        ),
                        BranchUpdateError::ValidationFailed(branch_name) => {
                            auto_build_push_failed_comment(&format!(
                                "the tested commit was behind the `{branch_name}` branch"
                            ))
                        }
                        error => auto_build_push_failed_comment(&error.to_string()),
                    };

                    ctx.db
                        .update_build_status(auto_build, BuildStatus::Failure)
                        .await?;

                    repo.client.post_comment(pr_num, comment).await?;
                } else {
                    tracing::info!("Auto build succeeded and merged for PR {pr_num}");

                    ctx.db
                        .set_pr_status(&pr.repository, pr.number, PullRequestStatus::Merged)
                        .await?;

                    repo.client.post_comment(pr.number, comment).await?;
                }

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

    Ok(())
}

#[must_use]
pub enum StartAutoBuildError {
    /// Merge conflict between PR head and base branch.
    MergeConflict,
    /// Failed to perform required database operation.
    DatabaseError(anyhow::Error),
    /// GitHub API error.
    GitHubError(anyhow::Error),
    /// Sanity checks failed - PR state doesn't match requirements.
    SanityCheckFailed(anyhow::Error),
}

async fn verify_pr_state(gh_pr: &PullRequest, pr: &PullRequestModel) -> anyhow::Result<()> {
    let approved_sha = pr
        .approved_sha()
        .ok_or_else(|| anyhow::anyhow!("PR is not approved"))?;

    anyhow::ensure!(
        gh_pr.head.sha == CommitSha(approved_sha.to_string()),
        "PR head SHA {} does not match approved SHA {approved_sha}",
        gh_pr.head.sha
    );
    anyhow::ensure!(
        gh_pr.mergeable_state == OctocrabMergeableState::Clean,
        "PR is not mergeable, mergeability status: {:?}",
        gh_pr.mergeable_state
    );
    anyhow::ensure!(
        gh_pr.status == PullRequestStatus::Open,
        "PR is not open, status: {:?}",
        gh_pr.status
    );
    Ok(())
}

/// Starts a new auto build for a pull request.
async fn start_auto_build(
    repo: &RepositoryState,
    ctx: &BorsContext,
    pr: &PullRequestModel,
) -> anyhow::Result<(), StartAutoBuildError> {
    let client = &repo.client;

    let gh_pr = client
        .get_pull_request(pr.number)
        .await
        .map_err(StartAutoBuildError::GitHubError)?;
    let base_sha = client
        .get_branch_sha(&pr.base_branch)
        .await
        .map_err(StartAutoBuildError::GitHubError)?;
    let head_sha = gh_pr.head.sha.clone();

    verify_pr_state(&gh_pr, pr)
        .await
        .map_err(StartAutoBuildError::SanityCheckFailed)?;

    let pr_data = super::handlers::PullRequestData {
        db: pr,
        github: &gh_pr,
    };

    let auto_merge_commit_message = create_merge_commit_message(pr_data, MergeType::Auto);

    // 1. Merge PR head with base branch on `AUTO_MERGE_BRANCH_NAME`
    let merge_sha = match attempt_merge(
        client,
        AUTO_MERGE_BRANCH_NAME,
        &head_sha,
        &base_sha,
        &auto_merge_commit_message,
    )
    .await
    {
        Ok(MergeResult::Success(merge_sha)) => merge_sha,
        Ok(MergeResult::Conflict) => return Err(StartAutoBuildError::MergeConflict),
        Err(error) => return Err(StartAutoBuildError::GitHubError(error)),
    };

    // 2. Push merge commit to `AUTO_BRANCH_NAME` where CI runs
    client
        .set_branch_to_sha(AUTO_BRANCH_NAME, &merge_sha, ForcePush::Yes)
        .await
        .map_err(|e| StartAutoBuildError::GitHubError(e.into()))?;

    // 3. Record the build in the database
    let build_id = ctx
        .db
        .attach_auto_build(
            pr,
            AUTO_BRANCH_NAME.to_string(),
            merge_sha.clone(),
            base_sha,
        )
        .await
        .map_err(StartAutoBuildError::DatabaseError)?;

    // After this point, this function will always return Ok,
    // since the auto build has been started and recorded in the DB.

    // 4. Set GitHub check run to pending on PR head
    match client
        .create_check_run(
            AUTO_BUILD_CHECK_RUN_NAME,
            &head_sha,
            CheckRunStatus::InProgress,
            CheckRunOutput {
                title: AUTO_BUILD_CHECK_RUN_NAME.to_string(),
                summary: "".to_string(),
            },
            &build_id.to_string(),
        )
        .await
    {
        Ok(CheckRun { id, .. }) => {
            tracing::info!("Created check run {id} for build {build_id}");

            if let Err(error) = ctx
                .db
                .update_build_check_run_id(build_id, id.into_inner() as i64)
                .await
            {
                tracing::error!("Failed to update database with build check run id {id}: {error:?}",)
            };
        }
        Err(error) => {
            tracing::error!("Failed to create check run: {error:?}");
        }
    }

    // 5. Post status comment
    let comment = auto_build_started_comment(&head_sha, &merge_sha);
    if let Err(error) = client.post_comment(pr.number, comment).await {
        tracing::error!(
            "Failed to post auto build started comment on PR {}: {error:?}",
            pr.number
        );
    };

    Ok(())
}

/// Starts the background merge queue loop.
///
/// It receives events on the sender that it returns, and acts based on them.
/// It reacts to the following events:
/// - When `MaybePerformTick` is received, the queue checks if it has been notified.
///     - If yes, the merge queue runs a tick.
///     - If not, but `max_interval` has elapsed since the last tick, the merge queue runs a tick.
///     - Otherwise nothing happens.
/// - When `Notify` is performed, the queue stores the information that it has been notified, and
///   it will run on the next `MaybePerformTick` event.
/// - When `Shutdown` is received, the merge queue ends.
/// - When `PerformTick` is received, the merge queue tick runs. This is only used in tests.
///
/// This design is used to both ensure that the queue does not run too often (e.g. if there are
/// transient networking/database failures) nor too rarely.
pub fn start_merge_queue(
    ctx: Arc<BorsContext>,
    max_interval: chrono::Duration,
) -> (MergeQueueSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<MergeQueueEvent>(1024);
    let sender = MergeQueueSender { inner: tx };

    let mut notified = false;
    let mut last_executed_at = Utc::now() - max_interval;

    let fut = async move {
        async fn run_tick(
            ctx: &Arc<BorsContext>,
            notified: &mut bool,
            last_executed_at: &mut DateTime<Utc>,
        ) {
            *notified = false;
            *last_executed_at = Utc::now();

            let span = tracing::info_span!("MergeQueue");
            tracing::debug!("Processing merge queue");
            if let Err(error) = merge_queue_tick(ctx.clone()).instrument(span.clone()).await {
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

        while let Some(event) = rx.recv().await {
            match event {
                #[cfg(test)]
                MergeQueueEvent::PerformTick => {
                    run_tick(&ctx, &mut notified, &mut last_executed_at).await;
                    crate::bors::WAIT_FOR_MERGE_QUEUE.mark();
                }
                MergeQueueEvent::MaybePerformTick => {
                    if notified || (Utc::now() - last_executed_at) >= max_interval {
                        run_tick(&ctx, &mut notified, &mut last_executed_at).await;
                    }
                }
                MergeQueueEvent::Notify => {
                    notified = true;
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

    use crate::tests::{BorsBuilder, GitHubState, run_test};
    use crate::{
        bors::{
            PullRequestStatus,
            merge_queue::{AUTO_BRANCH_NAME, AUTO_BUILD_CHECK_RUN_NAME, AUTO_MERGE_BRANCH_NAME},
        },
        database::{BuildStatus, MergeableState, OctocrabMergeableState},
        github::CommitSha,
        tests::{BorsTester, BranchPushBehaviour, BranchPushError, Comment, default_repo_name},
    };

    #[sqlx::test]
    async fn disabled_merge_queue(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::default().with_default_config(
                r#"
merge_queue_enabled = false
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester.approve(()).await?;
                tester.process_merge_queue().await;
                // Check that no comments were sent and no builds were started
                assert!(
                    tester
                        .db()
                        .get_pending_builds(&default_repo_name())
                        .await?
                        .is_empty()
                );
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_check_run_created(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    CheckRunStatus::InProgress,
                    None,
                )
                .await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_started_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-0-pr-1..."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.workflow_full_success(tester.auto_branch().await).await?;
            tester.process_merge_queue().await;

            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
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
    async fn auto_build_failure_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.workflow_full_failure(tester.auto_branch().await).await?;

            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @":broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)"
            );
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_insert_into_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            assert!(
                tester
                    .db()
                    .find_build(
                        &default_repo_name(),
                        AUTO_BRANCH_NAME.to_string(),
                        CommitSha(tester.auto_branch().await.get_sha().to_string()),
                    )
                    .await?
                    .is_some()
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_succeeds_and_merges_in_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.finish_auto_build(()).await?;
            tester
                .get_pr_copy(())
                .await
                .expect_status(PullRequestStatus::Merged)
                .expect_auto_build(|b| b.status == BuildStatus::Success);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_branch_history(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.finish_auto_build(()).await?;
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
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            let prs = [
                tester.get_pr_copy(()).await.get_gh_pr(),
                tester.open_pr(default_repo_name(), |_| {}).await?,
                tester.open_pr(default_repo_name(), |_| {}).await?,
                tester.open_pr(default_repo_name(), |_| {}).await?,
            ];

            // Approve the PRs
            for pr in &prs {
                tester.approve(pr.id()).await?;
            }

            // Check that they were merged in order by PR number
            for pr in &prs {
                tester.start_and_finish_auto_build(pr.id()).await?;
            }

            Ok(())
        })
        .await;

        gh.check_sha_history(
            default_repo_name(),
            "main",
            &[
                "main-sha1",
                "merge-0-pr-1",
                "merge-1-pr-2",
                "merge-2-pr-3",
                "merge-3-pr-4",
            ],
        );
    }

    #[sqlx::test]
    async fn merge_queue_priority_order(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |tester: &mut BorsTester| {
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr4 = tester.open_pr(default_repo_name(), |_| {}).await?;

            tester.approve(pr2.id()).await?;
            tester.approve(pr3.id()).await?;
            tester
                .post_comment(Comment::new(pr4.id(), "@bors r+ p=3"))
                .await?;
            tester.expect_comments(pr4.id(), 1).await;

            tester.start_and_finish_auto_build(pr4.id()).await?;
            tester.start_and_finish_auto_build(pr2.id()).await?;
            tester.start_and_finish_auto_build(pr3.id()).await?;

            Ok(())
        })
        .await;

        gh.check_sha_history(
            default_repo_name(),
            "main",
            &["main-sha1", "merge-0-pr-4", "merge-1-pr-2", "merge-2-pr-3"],
        );
    }

    #[sqlx::test]
    async fn auto_build_push_conflict(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
               .modify_repo(&default_repo_name(), |repo| {
                   repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::Conflict)
               })
               .await;
            tester.workflow_full_success(tester.auto_branch().await).await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: this PR has conflicts with the `main` branch"
            );
            Ok(())
       })
       .await;
    }

    #[sqlx::test]
    async fn auto_build_push_validation_failed(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::ValidationFailed)
                })
               .await;
            tester.workflow_full_success(tester.auto_branch().await).await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: the tested commit was behind the `main` branch"
            );
            Ok(())
       })
       .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_details_failed(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::always_fail(BranchPushError::InternalServerError)
                })
                .await;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: IO error"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_fails_in_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::always_fail(BranchPushError::Conflict)
                })
                .await;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.process_merge_queue().await;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_auto_build(|b| b.status == BuildStatus::Failure);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_retry_recovers_and_merges(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::fail_n_times(BranchPushError::InternalServerError, 1)
                })
                .await;

            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            // Check that the merge queue retries the push request
            tester.finish_auto_build(()).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_status(PullRequestStatus::Merged);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_start_merge_conflict_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .modify_branch(AUTO_MERGE_BRANCH_NAME, |branch| {
                    branch.merge_conflict = true;
                })
                .await;
            tester.approve(()).await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment_text(()).await?,
                @r#"
            :lock: Merge conflict

            This pull request and the master branch diverged in a way that cannot
             be automatically merged. Please rebase on top of the latest master
             branch, and let the reviewer approve again.

            <details><summary>How do I rebase?</summary>

            Assuming `self` is your fork and `upstream` is this repository,
             you can resolve the conflict following these steps:

            1. `git checkout pr-1` *(switch to your branch)*
            2. `git fetch upstream master` *(retrieve the latest master)*
            3. `git rebase upstream/master -p` *(rebase on top of it)*
            4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
            5. `git push self pr-1 --force-with-lease` *(update this PR)*

            You may also read
             [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
             for a short tutorial.

            Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
             It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

            Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
            handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

            </details>
            "#
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_start_merge_conflict_in_db(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .modify_branch(AUTO_MERGE_BRANCH_NAME, |branch| {
                    branch.merge_conflict = true;
                })
                .await;
            tester.approve(()).await?;
            tester.process_merge_queue().await;
            tester.expect_comments((), 1).await;
            tester
                .get_pr_copy(())
                .await
                .expect_mergeable_state(MergeableState::Unknown);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_mergeable_state_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.approve(pr.id()).await?;
            tester
                .modify_pr_state(pr.id(), |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                })
                .await;
            tester.process_merge_queue().await;
            tester.get_pr_copy(pr.id()).await.expect_no_auto_build();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_status_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.approve(pr.id()).await?;
            tester.modify_pr_state(pr.id(), |pr| pr.close_pr()).await;
            tester.process_merge_queue().await;
            tester.get_pr_copy(pr.id()).await.expect_no_auto_build();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_sha_mismatch_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.approve(pr.id()).await?;
            tester
                .edit_pr(pr.id(), |pr| {
                    pr.head_sha = "different-sha".to_string();
                })
                .await?;
            tester.process_merge_queue().await;
            tester.get_pr_copy(pr.id()).await.expect_no_auto_build();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_sanity_check_recovers(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester.approve(pr.id()).await?;
            tester.modify_pr_state(pr.id(), |pr| pr.close_pr()).await;
            tester.process_merge_queue().await;
            tester.get_pr_copy(pr.id()).await.expect_no_auto_build();
            tester.modify_pr_state(pr.id(), |pr| pr.open_pr()).await;
            tester.start_auto_build(pr.id()).await?;
            tester
                .get_pr_copy(pr.id())
                .await
                .expect_auto_build(|_| true);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_and_finish_auto_build(()).await?;
            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Success),
                )
                .await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_labels(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
merge_queue_enabled = true

[labels]
auto_build_succeeded = ["+foo", "+bar", "-baz"]
  "#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(async |tester: &mut BorsTester| {
                tester.approve(()).await?;
                tester.start_auto_build(()).await?;

                tester.get_pr_copy(()).await.expect_added_labels(&[]);
                tester.finish_auto_build(()).await?;

                tester
                    .get_pr_copy(())
                    .await
                    .expect_added_labels(&["foo", "bar"])
                    .expect_removed_labels(&["baz"]);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failed_labels(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
merge_queue_enabled = true

[labels]
auto_build_failed = ["+foo", "+bar", "-baz"]
      "#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(async |tester: &mut BorsTester| {
                tester.approve(()).await?;
                tester.start_auto_build(()).await?;

                tester.get_pr_copy(()).await.expect_added_labels(&[]);
                tester
                    .workflow_full_failure(tester.auto_branch().await)
                    .await?;
                tester.expect_comments((), 1).await;

                tester
                    .get_pr_copy(())
                    .await
                    .expect_added_labels(&["foo", "bar"])
                    .expect_removed_labels(&["baz"]);

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failure_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;

            tester
                .workflow_full_failure(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .expect_check_run(
                    &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Failure),
                )
                .await;
            Ok(())
        })
        .await;
    }
}
