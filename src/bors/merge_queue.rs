use anyhow::Context;
use chrono::{DateTime, Utc};
use octocrab::models::checks::CheckRun;
use octocrab::params::checks::CheckRunStatus;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use super::{MergeType, bors_commit_author, create_merge_commit_message};
use crate::bors::build::load_workflow_runs;
use crate::bors::comment::{
    auto_build_push_failed_comment, auto_build_started_comment, auto_build_succeeded_comment,
    unapproved_because_of_sha_mismatch_comment,
};
use crate::bors::handlers::unapprove_pr;
use crate::bors::mergeability_queue::{MergeabilityQueueSender, update_pr_with_known_mergeability};
use crate::bors::{AUTO_BRANCH_NAME, PullRequestStatus, RepositoryState};
use crate::database::{
    ApprovalInfo, BuildModel, BuildStatus, ExclusiveLockProof, ExclusiveOperationOutcome,
    MergeableState, PullRequestModel, QueueStatus, UpdateBuildParams,
};
use crate::github::api::client::CheckRunOutput;
use crate::github::api::operations::{BranchUpdateError, ForcePush};
use crate::github::{CommitSha, PullRequest, PullRequestNumber};
use crate::github::{MergeResult, attempt_merge};
use crate::utils::sort_queue::sort_queue_prs;
use crate::{BorsContext, PgDbClient};

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
}

/// Branch used for performing merge operations.
/// This branch should not run CI checks.
pub(super) const AUTO_MERGE_BRANCH_NAME: &str = "automation/bors/auto-merge";

// The name of the check run seen in the GitHub UI.
pub(super) const AUTO_BUILD_CHECK_RUN_NAME: &str = "Bors auto build";

/// Process the merge queue.
/// Try to finish and merge a successful auto build, if any.
/// If there is a PR ready to be merged, starts an auto build for it.
pub async fn merge_queue_tick(
    ctx: Arc<BorsContext>,
    mergeability_sender: &MergeabilityQueueSender,
) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> = ctx.repositories.repositories();

    for repo in repos {
        let repo_name = repo.repository().to_string();
        // We need to hold the lock over the whole merge queue operation, otherwise we would
        // protect only the merge operations, but two concurrent bors instances could still start
        // an auto build after one another (if the second instance didn't see the updated PR state
        // from the DB).
        let res = ctx
            .db
            .ensure_not_concurrent(
                &format!("{}-auto-build", repo.repository()),
                async |proof| {
                    if let Err(error) =
                        process_repository(&repo, &ctx, mergeability_sender, proof).await
                    {
                        tracing::error!("Error running merge queue for {repo_name}: {error:?}");
                    }
                },
            )
            .await
            .context("Merge lock failure")?;
        match res {
            ExclusiveOperationOutcome::Performed(_) => {}
            ExclusiveOperationOutcome::Skipped => {
                tracing::warn!(
                    "Merge queue tick was not performed due to other concurrent bors instance"
                );
            }
        }
    }

    #[cfg(test)]
    crate::bors::WAIT_FOR_MERGE_QUEUE.mark();

    Ok(())
}

async fn process_repository(
    repo: &RepositoryState,
    ctx: &BorsContext,
    mergeability_sender: &MergeabilityQueueSender,
    proof: ExclusiveLockProof,
) -> anyhow::Result<()> {
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

    let tree_min_priority = repo_db.tree_state.priority();
    let prs = ctx.db.get_nonclosed_pull_requests(repo_name).await?;

    // Sort PRs according to merge queue priority rules.
    // Successful builds come first so they can be merged immediately,
    // then pending builds (which block the queue to prevent starting simultaneous auto-builds).
    let prs = sort_queue_prs(prs);

    for pr in &prs {
        let pr_num = pr.number;

        match pr.queue_status() {
            QueueStatus::Pending(..) => {
                // Build in progress - stop queue. We can only have one PR being built
                // at a time.
                tracing::info!("PR {pr_num} has a pending build - blocking queue");
                break;
            }
            QueueStatus::ReadyForMerge(approval_info, auto_build) => {
                #[cfg(test)]
                crate::bors::WAIT_FOR_MERGE_QUEUE_MERGE_ATTEMPT.mark();

                handle_successful_build(repo, ctx, pr, auto_build, approval_info, pr_num).await?;
                break;
            }
            QueueStatus::Approved(approval_info) => {
                // Respect tree closure
                if let Some(tree_min_priority) = tree_min_priority
                    && tree_min_priority > pr.priority.unwrap_or(0) as u32
                {
                    // End processing the queue here; all following PRs should have a lower
                    // priority
                    break;
                }

                // Note that we do NOT check the mergeability status from the DB here.
                // Unmergeable PRs should be ordered after mergeable ones, but we still try to merge
                // approved unmergeable PRs, just in case our DB data is stale. That should
                // hopefully happen only very rarely.
                tracing::info!(
                    "Attempting to start auto build for {pr_num}. Current queue: {prs:?}"
                );
                match handle_start_auto_build(
                    repo,
                    ctx,
                    pr,
                    approval_info,
                    mergeability_sender,
                    &proof,
                )
                .await?
                {
                    AutoBuildStartOutcome::BuildStarted | AutoBuildStartOutcome::PauseQueue => {
                        break;
                    }
                    AutoBuildStartOutcome::ContinueToNextPr => {
                        tracing::warn!("Continuing to next pull request in the merge queue");
                    }
                }
            }
            // We got to the end of the merge queue, stop going through the rest of the PRs
            QueueStatus::NotApproved | QueueStatus::Failed(..) => break,
        }
    }

    Ok(())
}

/// Return a pull request that will be likely tested the next time the merge queue runs (if any).
pub async fn get_pr_at_front_of_merge_queue(
    repo_state: &RepositoryState,
    db: &PgDbClient,
) -> anyhow::Result<Option<PullRequestModel>> {
    let prs = db
        .get_nonclosed_pull_requests(repo_state.repository())
        .await?;
    Ok(sort_queue_prs(prs).into_iter().next())
}

/// Handle a successful auto build by pointing the base branch to the merged commit.
async fn handle_successful_build(
    repo: &RepositoryState,
    ctx: &BorsContext,
    pr: &PullRequestModel,
    auto_build: &BuildModel,
    approval_info: &ApprovalInfo,
    pr_num: PullRequestNumber,
) -> anyhow::Result<()> {
    let commit_sha = CommitSha(auto_build.commit_sha.clone());
    let workflow_runs = load_workflow_runs(repo, &ctx.db, auto_build)
        .await
        .context("Cannot load workflow runs")?;
    let comment = auto_build_succeeded_comment(
        workflow_runs,
        &approval_info.approver,
        &commit_sha,
        &pr.base_branch,
    );

    if let Err(error) = repo
        .client
        .set_branch_to_sha(&pr.base_branch, &commit_sha, ForcePush::No)
        .await
    {
        tracing::error!("Failed to fast-forward base branch for PR {pr_num}: {error:?}");

        let error_comment = match &error {
            BranchUpdateError::Conflict(branch_name) => Some(auto_build_push_failed_comment(
                &format!("this PR has conflicts with the `{branch_name}` branch"),
            )),
            BranchUpdateError::ValidationFailed(branch_name) => {
                Some(auto_build_push_failed_comment(&format!(
                    "the tested commit was behind the `{branch_name}` branch"
                )))
            }
            // If a transient error happened, try again next time
            _ => None,
        };

        if let Some(error_comment) = error_comment {
            ctx.db
                .update_build(
                    auto_build.id,
                    UpdateBuildParams::default().status(BuildStatus::Failure),
                )
                .await?;
            repo.client
                .post_comment(pr_num, error_comment, &ctx.db)
                .await?;
        }
    } else {
        tracing::info!("Auto build succeeded and merged for PR {pr_num}");
        ctx.db
            .set_pr_status(&pr.repository, pr.number, PullRequestStatus::Merged)
            .await?;
        repo.client
            .post_comment(pr.number, comment, &ctx.db)
            .await?;
    }

    Ok(())
}

enum AutoBuildStartOutcome {
    /// The auto build has been started.
    BuildStarted,
    /// The pull request was kicked out of the queue, continue to the next PR.
    ContinueToNextPr,
    /// There was some transient error, the queue should be paused for a bit.
    PauseQueue,
}

/// Handle starting a new auto build for an approved PR.
#[tracing::instrument(skip(repo, ctx, pr, mergeability_sender, proof))]
async fn handle_start_auto_build(
    repo: &RepositoryState,
    ctx: &BorsContext,
    pr: &PullRequestModel,
    approval_info: &ApprovalInfo,
    mergeability_sender: &MergeabilityQueueSender,
    proof: &ExclusiveLockProof,
) -> anyhow::Result<AutoBuildStartOutcome> {
    let pr_num = pr.number;
    if let MergeableState::HasConflicts = pr.mergeable_status() {
        tracing::warn!("Attempting to start auto build for unmergeable PR {pr_num}");
    }

    let Err(error) = start_auto_build(repo, ctx, pr, approval_info, proof).await else {
        tracing::info!("Started auto build for PR {pr_num}");
        return Ok(AutoBuildStartOutcome::BuildStarted);
    };

    match error {
        StartAutoBuildError::MergeConflict(mut gh_pr) => {
            tracing::debug!("Failed to start auto build for PR {pr_num} due to merge conflict");

            // Regardless of what the GH state said, we now know that the PR is not mergeable
            gh_pr.mergeable_state = octocrab::models::pulls::MergeableState::Dirty;
            update_pr_with_known_mergeability(
                repo,
                &ctx.db,
                &gh_pr,
                pr,
                mergeability_sender.get_conflict_source(pr),
            )
            .await?;
            Ok(AutoBuildStartOutcome::ContinueToNextPr)
        }
        StartAutoBuildError::SanityCheckFailed {
            error: SanityCheckError::WrongStatus { status },
            pr: _,
        } => {
            tracing::info!("Sanity check failed for PR {pr_num}: its status was {status}");
            // The DB PR status was wrong, let's update it.
            // We don't even send a comment, since the PR is either closed or already merged.
            ctx.db
                .set_pr_status(&pr.repository, pr.number, status)
                .await?;
            Ok(AutoBuildStartOutcome::ContinueToNextPr)
        }
        StartAutoBuildError::SanityCheckFailed {
            error: SanityCheckError::NotMergeable { mergeable_state },
            pr: gh_pr,
        } => {
            tracing::info!(
                "Sanity check failed for PR {pr_num}: it was not mergeable (mergeable state: {mergeable_state:?})"
            );
            // Either the mergeability status is unknown, which can most often happen if another PR
            // was *just* merged, and the base branch was pushed to, or it is unmergeable.
            let outcome = match mergeable_state {
                MergeableState::Mergeable => {
                    unreachable!("Mergeable status received as the result of a failed sanity check")
                }
                // If the PR has conflicts on GitHub, we have several choices:
                // 1. Put the PR into the mergeability queue. This will post the unmergeable
                //   notification at a later time, but it could have higher quality message, since
                //   it might know the conflict source, and it will also notify the PR that it has
                //   been unapproved in a single comment.
                // 2. Immediately post the unmergeable notification here. This ensures that we
                //   unapprove and send the notification at the same time, but we might lose the
                //   conflict source.
                // 3. Unapprove immediately to remove the PR from the merge queue, but still enqueue
                //   the PR into the mergeability queue. In that case, the unapproval will happen
                //   without a comment, and (hopefully) soon after the PR should receive the
                //   unmergeable notification.
                // To try to combine the benefits of 1. and 2., we use a combined approach.
                // We unapprove the PR immediately, to remove it from the merge queue, and also
                // immediately send the unmergeability notification. However, we also try to lookup
                // a potential existing conflict source from the mergeability queue, to avoid losing
                // the detailed context.
                MergeableState::HasConflicts => {
                    update_pr_with_known_mergeability(
                        repo,
                        &ctx.db,
                        &gh_pr,
                        pr,
                        mergeability_sender.get_conflict_source(pr),
                    )
                    .await?;
                    // We continue with the rest of the queue, since we know that this
                    // PR was unapproved above.
                    AutoBuildStartOutcome::ContinueToNextPr
                }
                // If it's unknown, we want to wait until the mergeability queue reloads the status,
                // so we pause the merge queue.
                // Note that if a PR is already queued with a known conflict source, this will
                // **NOT** overwrite the conflict source with `None`.
                MergeableState::Unknown => {
                    mergeability_sender.enqueue_pr(pr, None);
                    AutoBuildStartOutcome::PauseQueue
                }
            };

            Ok(outcome)
        }
        StartAutoBuildError::SanityCheckFailed {
            error: SanityCheckError::ApprovedShaMismatch { approved, actual },
            pr: gh_pr,
        } => {
            tracing::info!(
                "Sanity check failed for PR {pr_num}: approved SHA ({approved}) did not match actual SHA ({actual})"
            );
            // The PR head SHA does not match the approved SHA.
            // This should only happen if we missed a PR push webhook (which would normally unapprove
            // the PR). Let's unapprove it here instead to unblock the queue.
            unapprove_pr(repo, &ctx.db, pr, &gh_pr).await?;
            repo.client
                .post_comment(
                    pr.number,
                    unapproved_because_of_sha_mismatch_comment(
                        &CommitSha(pr.approved_sha().unwrap_or("<missing>").to_owned()),
                        &gh_pr.head.sha,
                    ),
                    &ctx.db,
                )
                .await?;
            Ok(AutoBuildStartOutcome::ContinueToNextPr)
        }
        StartAutoBuildError::GitHubError(error) => {
            tracing::debug!(
                "Failed to start auto build for PR {pr_num} due to a GitHub error: {error:?}"
            );
            Ok(AutoBuildStartOutcome::PauseQueue)
        }
        StartAutoBuildError::DatabaseError(error) => {
            tracing::debug!(
                "Failed to start auto build for PR {pr_num} due to database error: {error:?}"
            );
            Ok(AutoBuildStartOutcome::PauseQueue)
        }
    }
}

#[must_use]
enum StartAutoBuildError {
    /// Merge conflict between PR head and base branch.
    MergeConflict(PullRequest),
    /// Failed to perform required database operation.
    DatabaseError(anyhow::Error),
    /// GitHub API error.
    GitHubError(anyhow::Error),
    /// Sanity checks failed - PR state doesn't match requirements.
    SanityCheckFailed {
        error: SanityCheckError,
        pr: PullRequest,
    },
}

enum SanityCheckError {
    /// The pull request head SHA (actual) was different than the approved SHA.
    ApprovedShaMismatch {
        approved: CommitSha,
        actual: CommitSha,
    },
    /// The pull request was not mergeable.
    NotMergeable { mergeable_state: MergeableState },
    /// The pull request was not open.
    WrongStatus { status: PullRequestStatus },
}

async fn sanity_check_pr(
    gh_pr: &PullRequest,
    approval_info: &ApprovalInfo,
) -> Result<(), SanityCheckError> {
    if gh_pr.status != PullRequestStatus::Open {
        return Err(SanityCheckError::WrongStatus {
            status: gh_pr.status,
        });
    }

    let mergeable_state = MergeableState::from(gh_pr.mergeable_state.clone());
    if mergeable_state != MergeableState::Mergeable {
        return Err(SanityCheckError::NotMergeable { mergeable_state });
    }

    if gh_pr.head.sha.as_ref() != approval_info.sha {
        return Err(SanityCheckError::ApprovedShaMismatch {
            approved: CommitSha(approval_info.sha.clone()),
            actual: gh_pr.head.sha.clone(),
        });
    }
    Ok(())
}

/// Starts a new auto build for a pull request.
async fn start_auto_build(
    repo: &RepositoryState,
    ctx: &BorsContext,
    pr: &PullRequestModel,
    approval_info: &ApprovalInfo,
    proof: &ExclusiveLockProof,
) -> anyhow::Result<(), StartAutoBuildError> {
    let client = &repo.client;

    let gh_pr = client
        .get_pull_request(pr.number)
        .await
        .map_err(StartAutoBuildError::GitHubError)?;

    sanity_check_pr(&gh_pr, approval_info)
        .await
        .map_err(|error| StartAutoBuildError::SanityCheckFailed {
            error,
            pr: gh_pr.clone(),
        })?;

    let base_sha = client
        .get_branch_sha(&pr.base_branch)
        .await
        .map_err(StartAutoBuildError::GitHubError)?;
    let head_sha = gh_pr.head.sha.clone();

    let pr_data = super::handlers::PullRequestData {
        db: pr,
        github: &gh_pr,
    };

    let auto_merge_commit_message = create_merge_commit_message(pr_data, MergeType::Auto);

    // 1. Merge PR head with base branch on `AUTO_MERGE_BRANCH_NAME`
    // Use a temporary message to not reference/spam any issue
    let merged_commit = match attempt_merge(
        client,
        AUTO_MERGE_BRANCH_NAME,
        &head_sha,
        &base_sha,
        "merge commit",
        proof,
    )
    .await
    {
        Ok(MergeResult::Success(merged_commit)) => merged_commit,
        Ok(MergeResult::Conflict) => return Err(StartAutoBuildError::MergeConflict(gh_pr)),
        Err(error) => return Err(StartAutoBuildError::GitHubError(error)),
    };
    // Then, create the actual merge commit with an explicit author, so that we can override
    // the author information to the bors account, to keep compatibility with various tools
    // that depend on it.
    let merged_commit = repo
        .client
        .create_commit(
            &merged_commit.tree,
            &merged_commit.parents,
            &auto_merge_commit_message,
            &bors_commit_author(),
        )
        .await
        .map_err(|error| StartAutoBuildError::GitHubError(anyhow::anyhow!("{error}")))?;

    // 2. Push that merge commit to `AUTO_BRANCH_NAME` where CI runs
    client
        .set_branch_to_sha(AUTO_BRANCH_NAME, &merged_commit, ForcePush::Yes)
        .await
        .map_err(|e| StartAutoBuildError::GitHubError(e.into()))?;

    // 3. Record the build in the database
    let build_id = ctx
        .db
        .attach_auto_build(
            pr,
            AUTO_BRANCH_NAME.to_string(),
            merged_commit.clone(),
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
                .update_build(
                    build_id,
                    UpdateBuildParams::default().check_run_id(id.into_inner() as i64),
                )
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
    let comment = auto_build_started_comment(&head_sha, &merged_commit);
    if let Err(error) = client.post_comment(pr.number, comment, &ctx.db).await {
        tracing::error!("Failed to post auto build started comment: {error:?}");
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
    mergeability_sender: MergeabilityQueueSender,
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
            mergeability_sender: &MergeabilityQueueSender,
        ) {
            *notified = false;
            *last_executed_at = Utc::now();

            let span = tracing::info_span!("MergeQueue");
            tracing::debug!("Processing merge queue");
            if let Err(error) = merge_queue_tick(ctx.clone(), mergeability_sender)
                .instrument(span.clone())
                .await
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

        while let Some(event) = rx.recv().await {
            match event {
                #[cfg(test)]
                MergeQueueEvent::PerformTick => {
                    run_tick(
                        &ctx,
                        &mut notified,
                        &mut last_executed_at,
                        &mergeability_sender,
                    )
                    .await;
                }
                MergeQueueEvent::MaybePerformTick => {
                    // Note: this is not executed at all in tests
                    if notified || (Utc::now() - last_executed_at) >= max_interval {
                        run_tick(
                            &ctx,
                            &mut notified,
                            &mut last_executed_at,
                            &mergeability_sender,
                        )
                        .await;
                    }
                }
                MergeQueueEvent::Notify => {
                    notified = true;
                }
            }
        }
    };

    (sender, fut)
}

#[cfg(test)]
mod tests {
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
    use std::time::Duration;

    use crate::bors::with_mocked_time;
    use crate::github::api::client::HideCommentReason;
    use crate::tests::{BorsBuilder, GitHub, run_test};
    use crate::tests::{default_branch_name, default_repo_name};
    use crate::{
        bors::{
            PullRequestStatus,
            merge_queue::{AUTO_BRANCH_NAME, AUTO_BUILD_CHECK_RUN_NAME, AUTO_MERGE_BRANCH_NAME},
        },
        database::{BuildStatus, MergeableState, OctocrabMergeableState},
        tests::{BorsTester, BranchPushBehaviour, BranchPushError, Comment},
    };

    #[sqlx::test]
    async fn disabled_merge_queue(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
merge_queue_enabled = false
"#,
            ))
            .run_test(async |ctx: &mut BorsTester| {
                ctx.approve(()).await?;
                ctx.run_merge_queue_now().await;
                // Check that no comments were sent and no builds were started
                assert!(
                    ctx.db()
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
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.expect_check_run(
                &ctx.pr(()).await.get_gh_pr().head_sha,
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
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.run_merge_queue_now().await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-0-pr-1-d7d45f1f-reauthored-to-bors..."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_comment(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.workflow_full_success(ctx.auto_workflow()).await?;
            ctx.run_merge_queue_until_merge_attempt().await;

            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @r#"
            :sunny: Test successful - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)
            Approved by: `default-user`
            Pushing merge-0-pr-1-d7d45f1f-reauthored-to-bors to `main`...
            <!-- homu: {"type":"BuildCompleted","base_ref":"main","merge_sha":"merge-0-pr-1-d7d45f1f-reauthored-to-bors"} -->
            "#
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_failure_comment(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.workflow_full_failure(ctx.auto_workflow()).await?;

            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":broken_heart: Test for merge-0-pr-1-d7d45f1f-reauthored-to-bors failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)"
            );
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_insert_into_db(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            assert!(
                ctx.db()
                    .find_build(
                        &default_repo_name(),
                        AUTO_BRANCH_NAME,
                        ctx.auto_branch().get_commit().commit_sha(),
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
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_and_finish_auto_build(()).await?;
            ctx.pr(())
                .await
                .expect_status(PullRequestStatus::Merged)
                .expect_auto_build(|b| b.status == BuildStatus::Success);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_branch_history(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_and_finish_auto_build(()).await?;
            Ok(())
        })
        .await;
        insta::assert_snapshot!(gh.get_sha_history(
            (),
            "main",
        ), @"
        main-sha1
        merge-0-pr-1-d7d45f1f-reauthored-to-bors
        ");
        insta::assert_snapshot!(gh.get_sha_history(
            (),
            AUTO_MERGE_BRANCH_NAME
        ), @"
        main-sha1
        merge-0-pr-1-d7d45f1f
        ");
        insta::assert_snapshot!(gh.get_sha_history(
            (),
            AUTO_BRANCH_NAME,
        ), @"merge-0-pr-1-d7d45f1f-reauthored-to-bors");
    }

    #[sqlx::test]
    async fn auto_build_commit_author(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_and_finish_auto_build(()).await?;
            Ok(())
        })
        .await;
        let branch = gh
            .default_repo()
            .lock()
            .get_branch_by_name(default_branch_name())
            .unwrap()
            .clone();
        insta::assert_debug_snapshot!(branch.get_commit().author(), @r#"
        GitUser {
            name: "bors",
            email: "bors@rust-lang.org",
        }
        "#);
    }

    /// Make sure that when running bors concurrently, we will not start an auto build for a PR
    /// more than once.
    #[sqlx::test]
    async fn auto_build_concurrent_bors_instances(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            let concurrent_futs = (0..10)
                .map(|_| ctx.run_merge_queue_directly())
                .collect::<Vec<_>>();
            futures::future::join_all(concurrent_futs).await;

            assert_eq!(ctx.auto_branch().get_commit_history().len(), 1);
            ctx.pr(())
                .await
                .expect_auto_build(|build| build.status == BuildStatus::Pending);
            ctx.expect_comments((), 1).await;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn merge_queue_sequential_order(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |ctx: &mut BorsTester| {
            let prs = [
                ctx.pr(()).await.get_gh_pr(),
                ctx.open_pr((), |_| {}).await?,
                ctx.open_pr((), |_| {}).await?,
                ctx.open_pr((), |_| {}).await?,
            ];

            // Approve the PRs
            for pr in &prs {
                ctx.approve(pr.id()).await?;
            }

            // Check that they were merged in order by PR number
            for pr in &prs {
                ctx.start_and_finish_auto_build(pr.id()).await?;
            }

            Ok(())
        })
        .await;

        insta::assert_snapshot!(gh.get_sha_history(
            (),
            "main",

        ), @"
        main-sha1
        merge-0-pr-1-d7d45f1f-reauthored-to-bors
        merge-1-pr-2-d7d45f1f-reauthored-to-bors
        merge-2-pr-3-d7d45f1f-reauthored-to-bors
        merge-3-pr-4-d7d45f1f-reauthored-to-bors
        ");
    }

    #[sqlx::test]
    async fn merge_queue_priority_order(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            let pr4 = ctx.open_pr((), |_| {}).await?;

            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;
            ctx.post_comment(Comment::new(pr4.id(), "@bors r+ p=3"))
                .await?;
            ctx.expect_comments(pr4.id(), 1).await;

            ctx.start_and_finish_auto_build(pr4.id()).await?;
            ctx.start_and_finish_auto_build(pr2.id()).await?;
            ctx.start_and_finish_auto_build(pr3.id()).await?;

            Ok(())
        })
        .await;

        insta::assert_snapshot!(gh.get_sha_history(
            (),
            "main",

        ), @"
        main-sha1
        merge-0-pr-4-d7d45f1f-reauthored-to-bors
        merge-1-pr-2-d7d45f1f-reauthored-to-bors
        merge-2-pr-3-d7d45f1f-reauthored-to-bors
        ");
    }

    #[sqlx::test]
    async fn auto_build_push_conflict(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx
               .modify_repo((), |repo| {
                   repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::Conflict)
               });
            ctx.workflow_full_success(ctx.auto_workflow()).await?;
            ctx.run_merge_queue_until_merge_attempt().await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: this PR has conflicts with the `main` branch"
            );
            Ok(())
       })
       .await;
    }

    #[sqlx::test]
    async fn auto_build_push_validation_failed(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx
                .modify_repo((), |repo| {
                    repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::ValidationFailed)
                });
            ctx.workflow_full_success(ctx.auto_workflow()).await?;
            ctx.run_merge_queue_until_merge_attempt().await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":eyes: Test was successful, but fast-forwarding failed: the tested commit was behind the `main` branch"
            );
            Ok(())
       })
       .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_transient_error(pool: sqlx::PgPool) {
        let gh = run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.modify_repo((), |repo| {
                repo.push_behaviour =
                    BranchPushBehaviour::always_fail(BranchPushError::InternalServerError)
            });
            ctx.workflow_full_success(ctx.auto_workflow()).await?;
            ctx.run_merge_queue_until_merge_attempt().await;
            // Not comment should be posted, PR should not be merged
            Ok(())
        })
        .await;
        insta::assert_snapshot!(gh.get_sha_history((), default_branch_name()), @"main-sha1");
    }

    #[sqlx::test]
    async fn auto_build_push_error_fails_in_db(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.modify_repo((), |repo| {
                repo.push_behaviour = BranchPushBehaviour::always_fail(BranchPushError::Conflict)
            });
            ctx.workflow_full_success(ctx.auto_workflow()).await?;

            ctx.run_merge_queue_until_merge_attempt().await;
            ctx.expect_comments((), 1).await;

            ctx.pr(())
                .await
                .expect_auto_build(|b| b.status == BuildStatus::Failure);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_push_error_retry_recovers_and_merges(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.modify_repo((), |repo| {
                repo.push_behaviour =
                    BranchPushBehaviour::always_fail(BranchPushError::InternalServerError)
            });

            ctx.workflow_full_success(ctx.auto_workflow()).await?;

            // This should fail
            ctx.run_merge_queue_until_merge_attempt().await;

            ctx.modify_repo((), |repo| {
                repo.push_behaviour = BranchPushBehaviour::success();
            });

            // Check that the merge queue retries the push request
            ctx.run_merge_queue_now().await;
            ctx.expect_comments((), 1).await;

            ctx.pr(()).await.expect_status(PullRequestStatus::Merged);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_start_merge_conflict(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx
                .modify_branch(AUTO_MERGE_BRANCH_NAME, |branch| {
                    branch.merge_conflict = true;
                });
            ctx.approve(()).await?;
            ctx.run_merge_queue_now().await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @"
            :umbrella: The latest upstream changes made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).

            This pull request was unapproved.
            "
            );
            ctx
                .pr(())
                .await
                .expect_mergeable_state(MergeableState::HasConflicts);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_mergeable_state_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr.id()).await?;
            ctx.modify_pr_in_gh(pr.id(), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });
            ctx.run_merge_queue_now().await;
            ctx.pr(pr.id())
                .await
                .expect_no_auto_build()
                .expect_unapproved();
            insta::assert_snapshot!(ctx.get_next_comment_text(pr.id()).await?, @"
            :umbrella: The latest upstream changes made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).

            This pull request was unapproved.
            ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_mergeable_state_sanity_check_fails_conflict_source(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.start_and_finish_auto_build(pr2.id()).await?;
            let commit = ctx.auto_branch().get_commit().clone();

            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr3.id()).await?;

            ctx.drain_mergeability_queue().await?;

            ctx.modify_pr_in_gh(pr3.id(), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });
            // Set conflict source and enqueue pr3
            ctx.push_to_branch(default_branch_name(), commit).await?;

            ctx.run_merge_queue_now().await;
            ctx.pr(pr3.id())
                .await
                .expect_no_auto_build()
                .expect_unapproved();

            // Ensure that the conflict source was taken from the mergeability queue
            insta::assert_snapshot!(ctx.get_next_comment_text(pr3.id()).await?, @"
            :umbrella: The latest upstream changes (presumably #2) made this pull request unmergeable. Please [resolve the merge conflicts](https://rustc-dev-guide.rust-lang.org/git.html#rebasing-and-conflicts).

            This pull request was unapproved.
            ");

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_status_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr.id()).await?;
            ctx.modify_pr_in_gh(pr.id(), |pr| pr.close());
            ctx.run_merge_queue_now().await;
            ctx.pr(pr.id())
                .await
                .expect_no_auto_build()
                .expect_status(PullRequestStatus::Closed);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_sha_mismatch_sanity_check_fails(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.modify_pr_in_gh((), |pr| pr.head_sha = format!("{}-modified", pr.head_sha));
            ctx.run_merge_queue_now().await;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            The pull request was unapproved, because its SHA did not match the approved SHA during a merge attempt.
            Approved commit SHA: pr-1-sha
            Actual head SHA: pr-1-sha-modified
            ");
            ctx.pr(()).await.expect_unapproved();

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_success_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_and_finish_auto_build(()).await?;
            ctx.expect_check_run(
                &ctx.pr(()).await.get_gh_pr().head_sha,
                AUTO_BUILD_CHECK_RUN_NAME,
                AUTO_BUILD_CHECK_RUN_NAME,
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Success),
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn auto_build_success_labels(pool: sqlx::PgPool) {
        let github = GitHub::default().append_to_default_config(
            r#"
[labels]
auto_build_succeeded = ["+foo", "+bar", "-baz"]
  "#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(async |ctx: &mut BorsTester| {
                ctx.edit_pr((), |pr| pr.labels.push("baz".to_string()))
                    .await?;

                ctx.approve(()).await?;
                ctx.start_auto_build(()).await?;

                ctx.pr(()).await.expect_added_labels(&[]);
                ctx.finish_auto_build(()).await?;

                ctx.pr(())
                    .await
                    .expect_added_labels(&["foo", "bar"])
                    .expect_removed_labels(&["baz"]);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failed_labels(pool: sqlx::PgPool) {
        let github = GitHub::default().append_to_default_config(
            r#"
[labels]
auto_build_failed = ["+foo", "+bar", "-baz"]
      "#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(async |ctx: &mut BorsTester| {
                ctx.edit_pr((), |pr| pr.labels.push("baz".to_string()))
                    .await?;

                ctx.approve(()).await?;
                ctx.start_auto_build(()).await?;

                ctx.pr(()).await.expect_added_labels(&[]);
                ctx.workflow_full_failure(ctx.auto_workflow()).await?;
                ctx.expect_comments((), 1).await;

                ctx.pr(())
                    .await
                    .expect_added_labels(&["foo", "bar"])
                    .expect_removed_labels(&["baz"]);

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failure_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            ctx.workflow_full_failure(ctx.auto_workflow()).await?;
            ctx.expect_comments((), 1).await;
            ctx.expect_check_run(
                &ctx.pr(()).await.get_gh_pr().head_sha,
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
    async fn finish_auto_build_while_tree_is_closed_1(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            // Start an auto build with the default priority (0)
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            // Now close the tree for priority below 100
            ctx.post_comment("@bors treeclosed=100").await?;
            ctx.expect_comments((), 1).await;

            // Then finish the auto build AFTER the tree has been closed and then
            // run the merge queue
            ctx.finish_auto_build(()).await?;

            // And ensure that the PR was indeed merged
            ctx.pr(())
                .await
                .expect_status(PullRequestStatus::Merged)
                .expect_auto_build(|b| b.status == BuildStatus::Success);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn finish_auto_build_while_tree_is_closed_2(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            // Start an auto build with the default priority (0)
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            // Finish the auto build BEFORE the tree has been closed, then close the tree,
            // and only then run the merge queue
            ctx.workflow_full_success(ctx.auto_workflow()).await?;

            ctx.post_comment("@bors treeclosed=100").await?;
            ctx.expect_comments((), 1).await;
            ctx.run_merge_queue_until_merge_attempt().await;
            ctx.expect_comments((), 1).await;

            // And ensure that the PR was indeed merged
            ctx.pr(())
                .await
                .expect_status(PullRequestStatus::Merged)
                .expect_auto_build(|b| b.status == BuildStatus::Success);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn tree_closed_merge_open(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors treeclosed=100").await?;
            ctx.expect_comments((), 1).await;

            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;

            // This should not start an auto build yet
            ctx.approve(()).await?;
            ctx.run_merge_queue_now().await;
            ctx.pr(()).await.expect_no_auto_build();

            ctx.approve(pr3.id()).await?;
            ctx.post_comment(Comment::new(pr2.id(), "@bors r+ p=100"))
                .await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(pr2.id()).await?, @"
            :pushpin: Commit pr-2-sha has been approved by `default-user`

            It is now in the [queue](https://bors-test.com/queue/borstest) for this repository.
            ");

            ctx.start_and_finish_auto_build(pr2.id()).await?;

            ctx.post_comment("@bors treeopen").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"Tree is now open for merging.");

            ctx.start_and_finish_auto_build(()).await?;
            ctx.start_and_finish_auto_build(pr3.id()).await?;

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn run_empty_queue(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            // This PR should not be in the queue
            let pr = ctx.open_pr((), |_| {}).await?;
            // Make sure that bors knows about the DB
            ctx.post_comment(Comment::new(pr.id(), "@bors info"))
                .await?;
            ctx.expect_comments(pr.id(), 1).await;

            ctx.run_merge_queue_now().await;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn update_auto_build_started_comment_after_workflow_starts(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.run_merge_queue_now().await;
            let comment = ctx.get_next_comment(()).await?;

            ctx.workflow_start(ctx.auto_workflow()).await?;

            // Check that the comment text has been updated with a link to the started workflow
            let updated_comment = ctx
                .get_comment_by_node_id(&comment.node_id().unwrap())
                .unwrap();
            insta::assert_snapshot!(updated_comment.content(), @"
            :hourglass: Testing commit pr-1-sha with merge merge-0-pr-1-d7d45f1f-reauthored-to-bors...

            **Workflow**: https://github.com/rust-lang/borstest/actions/runs/1
            ");

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn hide_auto_build_started_comment_after_workflow_finish(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.run_merge_queue_now().await;
            let comment = ctx.get_next_comment(()).await?;

            ctx.workflow_full_failure(ctx.auto_workflow()).await?;
            ctx.expect_comments((), 1).await;
            ctx.expect_hidden_comment(&comment, HideCommentReason::Outdated);

            Ok(())
        })
        .await;
    }

    // Recover from a situation where an approved PR goes to the head of the queue, but it received
    // a new push in the meantime, but bors missed the webhook about that push.
    #[sqlx::test]
    async fn recover_approved_sha_mismatch_missed_webhook(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = ctx.open_pr(default_repo_name(), |_| {}).await?;

            // Approve both PRs
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            // Change its head SHA, but don't tell bors about it
            ctx
                .modify_pr_in_gh(pr2.id(), |pr| {
                    pr.head_sha = format!("{}-modified", pr.head_sha);
                });

            // Run the merge queue. It should recover and start merging PR3
            ctx.run_merge_queue_now().await;
            let comment = ctx.get_next_comment_text(pr3.id()).await?;
            insta::assert_snapshot!(comment, @":hourglass: Testing commit pr-3-sha with merge merge-0-pr-3-d7d45f1f-reauthored-to-bors...");

            // The merge queue should also unapprove PR2
            ctx.expect_comments(pr2.id(), 1).await;
            ctx.wait_for_pr(pr2.id(), |pr| !pr.is_approved()).await?;
            ctx.pr(pr2.id()).await.expect_no_auto_build();

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn recover_wrong_pr_status_missed_webhook(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = ctx.open_pr(default_repo_name(), |_| {}).await?;

            // Approve a PR
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            // Change its status, but don't tell bors about it
            ctx
                .modify_pr_in_gh(pr2.id(), |pr| {
                    pr.close();
                });

            // Run the merge queue. It should recover and start merging PR3
            ctx.run_merge_queue_now().await;
            let comment = ctx.get_next_comment_text(pr3.id()).await?;
            insta::assert_snapshot!(comment, @":hourglass: Testing commit pr-3-sha with merge merge-0-pr-3-d7d45f1f-reauthored-to-bors...");

            ctx.pr(pr2.id()).await.expect_no_auto_build();

            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn try_build_while_auto_build_is_running(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;
            ctx.workflow_full_success(ctx.try_workflow()).await?;
            ctx.expect_comments((), 1).await;
            ctx.finish_auto_build(()).await?;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn commit_message_skip_ignore_block(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.edit_pr((), |pr| {
                pr.description = r"This is a very good PR.

<!-- homu-ignore:start -->
ignore this 1
<!-- homu-ignore:end -->

include this

<!-- homu-ignore:start -->
ignore this 2
<!-- homu-ignore:end -->

also include this pls"
                    .to_string();
            })
            .await?;

            ctx.approve(()).await?;
            ctx.start_and_finish_auto_build(()).await?;

            insta::assert_snapshot!(ctx.auto_branch().get_commit().message(), @"
            Auto merge of #1 - pr-1, r=default-user

            Title of PR 1

            This is a very good PR.



            include this



            also include this pls
            ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_try_again(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;

            // Start a build
            ctx.start_auto_build(()).await?;
            ctx.workflow_start(ctx.auto_workflow()).await?;

            // Cancel it
            ctx.post_comment("@bors cancel").await?;
            ctx.expect_comments((), 1).await;

            // Start it again and finish it
            ctx.start_and_finish_auto_build(()).await?;

            ctx.pr(()).await.expect_status(PullRequestStatus::Merged);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_change_order(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(()).await?;
            ctx.approve(pr2.id()).await?;

            // Start a build
            ctx.start_auto_build(()).await?;
            ctx.workflow_start(ctx.auto_workflow()).await?;

            // Now increase the priority of the second PR
            ctx.post_comment(Comment::new(pr2.id(), "@bors p=1"))
                .await?;

            // Cancel the running build
            ctx.post_comment("@bors cancel").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            Auto build cancelled. Cancelled workflows:

            - https://github.com/rust-lang/borstest/actions/runs/1

            The next pull request likely to be tested is https://github.com/rust-lang/borstest/pull/2.
            ");

            // Now PR 2 should get priority
            ctx.start_and_finish_auto_build(pr2.id()).await?;
            ctx.start_and_finish_auto_build(()).await?;

            ctx.pr(()).await.expect_status(PullRequestStatus::Merged);
            ctx.pr(pr2.id())
                .await
                .expect_status(PullRequestStatus::Merged);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn do_not_merge_timed_out_pr(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;

            // Start a build
            ctx.start_auto_build(()).await?;
            ctx.workflow_start(ctx.auto_workflow()).await?;

            // Let it be timed out
            with_mocked_time(Duration::from_secs(7200), async {
                ctx.refresh_pending_builds().await;
                insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":boom: Test timed out after `3600`s");
                anyhow::Ok(())
            })
            .await?;
            // Ensure that the PR does not receive any more comments and that it is not tested again
            ctx.run_merge_queue_now().await;
            ctx.pr(()).await.expect_auto_build(|build| build.status == BuildStatus::Timeouted);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn wait_for_unknown_mergeability_status(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;

            ctx.approve(()).await?;
            ctx.modify_pr_in_gh((), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Unknown
            });

            // Try to merge, this shouldn't succeed, but it should add the PR to the mergeability
            // check queue. It should also not continue to PR2
            ctx.run_merge_queue_now().await;

            ctx.modify_pr_in_gh((), |pr| pr.mergeable_state = OctocrabMergeableState::Clean);
            ctx.refresh_mergeability_queue().await;
            ctx.wait_for_pr((), |pr| pr.mergeable_status() == MergeableState::Mergeable)
                .await?;

            ctx.start_and_finish_auto_build(()).await?;
            ctx.start_and_finish_auto_build(pr2.id()).await?;

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_conflicted_pr(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.post_comment(Comment::new(pr2.id(), "@bors r+ p=1"))
                .await?;
            ctx.expect_comments(pr2.id(), 1).await;
            ctx.approve(()).await?;

            ctx.modify_pr_in_gh(pr2.id(), |pr| {
                pr.mergeable_state = OctocrabMergeableState::Dirty;
            });

            // Skip PR2 and unapprove it
            ctx.start_and_finish_auto_build(()).await?;
            ctx.expect_comments(pr2.id(), 1).await;
            ctx.pr(pr2.id()).await.expect_unapproved();

            Ok(())
        })
        .await;
    }
}
