use anyhow::anyhow;
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
use crate::github::api::client::{CheckRunOutput, GithubRepositoryClient};
use crate::github::api::operations::{BranchUpdateError, ForcePush};
use crate::github::{CommitSha, MergeError, PullRequest};
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
        let repo_name = repo.repository().to_string();
        if let Err(error) = process_repository(&repo, &ctx).await {
            tracing::error!("Error processing repository {repo_name}: {error:?}");
        }
    }

    #[cfg(test)]
    crate::bors::WAIT_FOR_MERGE_QUEUE.mark();

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
                repo.client.post_comment(pr.number, comment).await?;

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
                                "this PR is behind the `{branch_name}` branch"
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
    anyhow::ensure!(
        gh_pr.mergeable_state == OctocrabMergeableState::Clean,
        "PR not mergeable"
    );
    anyhow::ensure!(gh_pr.status == PullRequestStatus::Open, "PR not opened");
    anyhow::ensure!(
        pr.approved_sha()
            .map(|sha| gh_pr.head.sha == CommitSha(sha.to_string()))
            .unwrap_or(false),
        "PR head sha doesn't match approved sha",
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
    let base_sha = gh_pr.base.sha.clone();
    let head_sha = gh_pr.head.sha.clone();

    verify_pr_state(&gh_pr, pr)
        .await
        .map_err(StartAutoBuildError::SanityCheckFailed)?;

    let auto_merge_commit_message = format!(
        "Auto merge of #{} - {}, r={}\n\n{}\n\n{}",
        pr.number,
        gh_pr.head_label,
        pr.approver().unwrap_or("<unknown>"),
        pr.title,
        gh_pr.message
    );

    // 1. Merge PR head with base branch on `AUTO_MERGE_BRANCH_NAME`
    let merge_sha =
        match attempt_merge(client, &head_sha, &base_sha, &auto_merge_commit_message).await {
            Ok(MergeResult::Success(merge_sha)) => merge_sha,
            Ok(MergeResult::Conflict) => {
                return Err(StartAutoBuildError::MergeConflict);
            }
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

    use octocrab::params::checks::CheckRunStatus;
    use sqlx::PgPool;

    use crate::{
        bors::{
            PullRequestStatus,
            merge_queue::{AUTO_BRANCH_NAME, AUTO_BUILD_CHECK_RUN_NAME, AUTO_MERGE_BRANCH_NAME},
        },
        database::{
            BuildStatus, MergeableState, OctocrabMergeableState, WorkflowStatus,
            operations::get_all_workflows,
        },
        github::CommitSha,
        tests::{
            BorsBuilder, BorsTester, BranchPushBehaviour, BranchPushError, Comment, GitHubState,
            WorkflowEvent, default_repo_name,
        },
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

    #[sqlx::test]
    async fn auto_workflow_started(pool: sqlx::PgPool) {
        run_merge_queue_test(pool.clone(), async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_event(WorkflowEvent::started(tester.auto_branch().await))
                .await?;
            Ok(())
        })
        .await;

        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn auto_workflow_check_run_created(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .expect_check_run(
                    &tester.get_pr(()).await.get_gh_pr().head_sha,
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
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-0-pr-1..."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_insert_into_db(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
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
    async fn auto_build_success_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester.workflow_full_success(tester.auto_branch().await).await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
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
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .wait_for_pr((), |pr| {
                    pr.auto_build.as_ref().unwrap().status == BuildStatus::Success
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.pr_status == PullRequestStatus::Merged)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_branch_history(pool: sqlx::PgPool) {
        let gh = run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
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
        let gh = run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = tester.open_pr(default_repo_name(), |_| {}).await?;

            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;

            tester.start_auto_build(pr2.number).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments(pr2.number, 1).await;

            tester.start_auto_build(pr3.number).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments(pr3.number, 1).await;

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
        let gh = run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = tester.open_pr(default_repo_name(), |_| {}).await?;

            tester.post_comment("@bors r+").await?;
            tester
                .post_comment(Comment::new(pr2.number, "@bors r+"))
                .await?;
            tester
                .post_comment(Comment::new(pr3.number, "@bors r+ p=3"))
                .await?;

            tester.expect_comments((), 1).await;
            tester.expect_comments(pr2.number, 1).await;
            tester.expect_comments(pr3.number, 1).await;

            tester.process_merge_queue().await;
            tester.expect_comments((), 1).await;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;

            tester.process_merge_queue().await;
            tester.expect_comments(pr3.number, 1).await;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments(pr3.number, 1).await;

            tester.process_merge_queue().await;
            tester.expect_comments(pr2.number, 1).await;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments(pr2.number, 1).await;

            Ok(())
        })
        .await;

        gh.check_sha_history(
            default_repo_name(),
            "main",
            &["main-sha1", "merge-0-pr-1", "merge-1-pr-3", "merge-2-pr-2"],
        );
    }

    #[sqlx::test]
    async fn auto_build_push_conflict_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
               tester.start_auto_build(()).await?;
               tester
                   .workflow_full_success(tester.auto_branch().await)
                   .await
?;
               tester.expect_comments((), 1).await;
               tester
                   .modify_repo(&default_repo_name(), |repo| {
                       repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::Conflict)
                   })
                   .await;
               tester.process_merge_queue().await;
               insta::assert_snapshot!(
                   tester.get_comment(()).await?,
                   @":eyes: Test was successful, but fast-forwarding failed: this PR has conflicts with the `main` branch"
               );
               Ok(())
           })
           .await;
    }

    #[sqlx::test]
    async fn auto_build_push_validation_failed_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
               tester.start_auto_build(()).await?;
               tester
                   .workflow_full_success(tester.auto_branch().await)
                   .await
?;
               tester.expect_comments((), 1).await;
               tester
                   .modify_repo(&default_repo_name(), |repo| {
                       repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::ValidationFailed)
                   })
                   .await;
               tester.process_merge_queue().await;
               insta::assert_snapshot!(
                   tester.get_comment(()).await?,
                   @":eyes: Test was successful, but fast-forwarding failed: this PR is behind the `main` branch"
               );
               Ok(())
           })
           .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_details_failed_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::always_fail(BranchPushError::InternalServerError)
                })
                .await;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: IO error"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_fails_in_db(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::always_fail(BranchPushError::Conflict)
                })
                .await;
            tester.process_merge_queue().await;
            tester.expect_comments((), 1).await;
            tester
                .wait_for_pr((), |pr| {
                    pr.auto_build.as_ref().unwrap().status == BuildStatus::Failure
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_retry_recovers_and_merges(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.start_auto_build(()).await?;
            tester
                .workflow_full_success(tester.auto_branch().await)
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.push_behaviour =
                        BranchPushBehaviour::fail_after(1, BranchPushError::InternalServerError)
                })
                .await;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr((), |pr| {
                    pr.auto_build.as_ref().unwrap().status == BuildStatus::Success
                })
                .await?;
            tester
                .wait_for_pr((), |pr| pr.pr_status == PullRequestStatus::Merged)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_start_merge_conflict_comment(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.create_branch(AUTO_MERGE_BRANCH_NAME).await;
            tester
                .modify_branch(AUTO_MERGE_BRANCH_NAME, |branch| {
                    branch.merge_conflict = true;
                })
                .await;
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
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
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            tester.create_branch(AUTO_MERGE_BRANCH_NAME).await;
            tester
                .modify_branch(AUTO_MERGE_BRANCH_NAME, |branch| {
                    branch.merge_conflict = true;
                })
                .await;
            tester.start_auto_build(()).await?;
            tester
                .wait_for_pr((), |pr| pr.mergeable_state == MergeableState::Unknown)
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_mergeable_state_sanity_check_fails(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .post_comment(Comment::new(pr.number, "@bors r+"))
                .await?;
            tester.expect_comments(pr.number, 1).await;
            tester
                .modify_pr_state(pr.number, |pr| {
                    pr.mergeable_state = OctocrabMergeableState::Dirty;
                })
                .await;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr(pr.number, |pr| pr.auto_build.is_none())
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_status_sanity_check_fails(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .post_comment(Comment::new(pr.number, "@bors r+"))
                .await?;
            tester.expect_comments(pr.number, 1).await;
            tester.modify_pr_state(pr.number, |pr| pr.close_pr()).await;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr(pr.number, |pr| pr.auto_build.is_none())
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_sha_mismatch_sanity_check_fails(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .post_comment(Comment::new(pr.number, "@bors r+"))
                .await?;
            tester.expect_comments(pr.number, 1).await;
            tester
                .edit_pr(pr.number, |pr| {
                    pr.head_sha = "different-sha".to_string();
                })
                .await?;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr(pr.number, |pr| pr.auto_build.is_none())
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_sanity_check_recovers(pool: sqlx::PgPool) {
        run_merge_queue_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.open_pr(default_repo_name(), |_| {}).await?;
            tester
                .post_comment(Comment::new(pr.number, "@bors r+"))
                .await?;
            tester.expect_comments(pr.number, 1).await;
            tester.modify_pr_state(pr.number, |pr| pr.close_pr()).await;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr(pr.number, |pr| pr.auto_build.is_none())
                .await?;
            tester.modify_pr_state(pr.number, |pr| pr.open_pr()).await;
            tester.process_merge_queue().await;
            tester
                .wait_for_pr(pr.number, |pr| pr.auto_build.is_some())
                .await?;
            tester.expect_comments(pr.number, 1).await;
            Ok(())
        })
        .await;
    }
}
