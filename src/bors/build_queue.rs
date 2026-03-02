//! The build queue is a background process that completes pending builds.
//! Rather than completing workflows in an edge-triggered manner (when workflow completion webhooks
//! arrive), we complete workflows using a serial queue. This has several benefits:
//! 1. There is exactly one place that completes builds, which helps to avoid race conditions
//! 2. If we miss a webhook about a workflow being completed, we can still complete its build later.
//!    This is useful especially when a long CI build actually finishes successfully, but its
//!    webhook is lost.
//!
//! Note: this queue is not just a global event because we do not want to block its working on the
//! other background sync processes. We want to finish builds as fast as possible.

use crate::bors::build::{
    CancelBuildConclusion, CancelBuildError, cancel_build, get_failed_jobs, load_workflow_runs,
};
use crate::bors::comment::{
    CommentTag, build_failed_comment, build_timed_out_comment, try_build_succeeded_comment,
};
use crate::bors::event::WorkflowRunCompleted;
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::unroll_queue::UnrollQueueSender;
use crate::bors::{
    BuildKind, Comment, FailedWorkflowRun, RepositoryState, elapsed_time_since,
    hide_tagged_comments,
};
use crate::database::{
    BuildModel, BuildStatus, PullRequestModel, UpdateBuildParams, WorkflowStatus,
};
use crate::github::{CommitSha, GithubRepoName, LabelTrigger};
use crate::{BorsContext, PgDbClient};
use anyhow::Context;
use octocrab::models::CheckRunId;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

pub type BuildQueueReceiver = mpsc::Receiver<BuildQueueEvent>;

#[derive(Clone)]
pub struct BuildQueueSender {
    inner: mpsc::Sender<BuildQueueEvent>,
}

impl BuildQueueSender {
    pub async fn refresh_pending_builds(
        &self,
        repo: GithubRepoName,
    ) -> Result<(), mpsc::error::SendError<BuildQueueEvent>> {
        self.inner
            .send(BuildQueueEvent::RefreshPendingBuilds(repo))
            .await
    }

    pub async fn on_workflow_completed(
        &self,
        event: WorkflowRunCompleted,
        error_context: Option<String>,
    ) -> Result<(), mpsc::error::SendError<BuildQueueEvent>> {
        // If we're in a test, we want to wait until the workflow completed event is actually
        // handled, to avoid possible race conditions.
        #[cfg(test)]
        crate::bors::WAIT_FOR_WORKFLOW_COMPLETED_HANDLED
            .drain()
            .await;

        self.inner
            .send(BuildQueueEvent::OnWorkflowCompleted {
                event,
                error_context,
            })
            .await?;

        #[cfg(test)]
        crate::bors::WAIT_FOR_WORKFLOW_COMPLETED_HANDLED
            .sync()
            .await;

        Ok(())
    }
}

pub fn create_build_queue() -> (BuildQueueSender, BuildQueueReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    (BuildQueueSender { inner: tx }, rx)
}

#[derive(Debug)]
pub enum BuildQueueEvent {
    RefreshPendingBuilds(GithubRepoName),
    OnWorkflowCompleted {
        event: WorkflowRunCompleted,
        error_context: Option<String>,
    },
}

pub async fn handle_build_queue_event(
    ctx: Arc<BorsContext>,
    event: BuildQueueEvent,
    merge_queue_tx: MergeQueueSender,
    unroll_queue_tx: UnrollQueueSender,
) -> anyhow::Result<()> {
    let db = &ctx.db;
    match event {
        BuildQueueEvent::RefreshPendingBuilds(repo) => {
            let repo = ctx.get_repo(&repo)?;
            let running_builds = db.get_pending_builds(repo.repository()).await?;
            tracing::info!("Found {} pending build(s)", running_builds.len());

            let timeout = repo.config.load().timeout;
            for build in running_builds {
                let handle = async {
                    let pr = db.find_pr_by_build(&build).await?;
                    // First try to complete builds, and only then timeout them.
                    // Because if the bot was offline for some time, we want to first attempt to
                    // actually finish the build, otherwise it might get instantly timeouted.
                    if !maybe_complete_build(
                        &repo,
                        db,
                        &build,
                        pr.as_ref(),
                        &merge_queue_tx,
                        &unroll_queue_tx,
                        None,
                    )
                    .await?
                    {
                        maybe_timeout_build(
                            &repo,
                            db,
                            &build,
                            pr.as_ref(),
                            timeout,
                            &unroll_queue_tx,
                        )
                        .await?;
                    }
                    anyhow::Ok(())
                };
                if let Err(error) = handle.await {
                    tracing::error!("Failed to handle pending build {build:?}: {error:?}")
                }
            }
        }
        BuildQueueEvent::OnWorkflowCompleted {
            event,
            error_context,
        } => {
            let handle = async {
                let build = db
                    .find_build(&event.repository, &event.branch, event.commit_sha.clone())
                    .await?;
                let Some(build) = build else {
                    return Ok(());
                };
                if build.status != BuildStatus::Pending {
                    tracing::warn!("Received workflow completed for an already completed build");
                    return Ok(());
                }
                let pr = db.find_pr_by_build(&build).await?;
                let repo = ctx.get_repo(&event.repository)?;
                maybe_complete_build(
                    &repo,
                    db,
                    &build,
                    pr.as_ref(),
                    &merge_queue_tx,
                    &unroll_queue_tx,
                    Some(CompletionTrigger { error_context }),
                )
                .await?;
                Ok(())
            };

            let res = handle.await;

            #[cfg(test)]
            crate::bors::WAIT_FOR_WORKFLOW_COMPLETED_HANDLED.mark();

            return res;
        }
    }

    Ok(())
}

async fn maybe_timeout_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
    pr: Option<&PullRequestModel>,
    timeout: Duration,
    unroll_queue_tx: &UnrollQueueSender,
) -> anyhow::Result<()> {
    if elapsed_time_since(build.created_at) >= timeout {
        tracing::info!("Cancelling build {build:?}");
        match cancel_build(&repo.client, db, build, CancelBuildConclusion::Timeout).await {
            Ok(_) => {}
            Err(
                CancelBuildError::FailedToMarkBuildAsCancelled(error)
                | CancelBuildError::FailedToCancelWorkflows(error),
            ) => {
                tracing::error!(
                    "Could not cancel workflows for SHA {}: {error:?}",
                    build.commit_sha
                );
            }
        }

        match build.kind {
            BuildKind::Try | BuildKind::Auto => {
                let Some(pr) = pr else {
                    return Ok(());
                };

                // Also handle label triggers
                let trigger = match build.kind {
                    BuildKind::Try => LabelTrigger::TryBuildFailed,
                    BuildKind::Auto => LabelTrigger::AutoBuildFailed,
                    BuildKind::TryPerf => unreachable!(),
                };
                let gh_pr = repo.client.get_pull_request(pr.number).await?;
                handle_label_trigger(repo, &gh_pr, trigger).await?;

                if let Err(error) = repo
                    .client
                    .post_comment(pr.number, build_timed_out_comment(timeout), db)
                    .await
                {
                    tracing::error!("Could not send comment to PR {}: {error:?}", pr.number);
                }
            }
            BuildKind::TryPerf => {
                if let Some(rollup_number) = db.find_rollup_for_perf_build(build.id).await? {
                    unroll_queue_tx
                        .enqueue_rollup(build.repository.clone(), rollup_number)
                        .await?;
                }
            }
        }
    }
    Ok(())
}

/// Represents data that triggered build queue completion handling.
/// Can be used to enrich the context of failure comments.
struct CompletionTrigger {
    /// An additional message that should be added to a comment if the build failed.
    error_context: Option<String>,
}

/// Attempt to complete a pending build.
/// If this completion handler was triggered in response to a workflow run being completed (in that
/// case `trigger` is `Some`), we assume that the status of the completed workflow run has already
/// been updated in the database.
///
/// We also assume that there is only a single check suite attached to a single build of a commit.
///
/// Returns true if the build was completed.
async fn maybe_complete_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
    pr: Option<&PullRequestModel>,
    merge_queue_tx: &MergeQueueSender,
    unroll_queue_tx: &UnrollQueueSender,
    completion_trigger: Option<CompletionTrigger>,
) -> anyhow::Result<bool> {
    assert_eq!(
        build.status,
        BuildStatus::Pending,
        "Attempting to complete a non-pending build"
    );

    let workflow_runs = load_workflow_runs(repo, db, build)
        .await
        .context("Cannot load workflow runs")?;

    // If we check build completion after a workflow run completion trigger,
    // there really should be at least a single workflow run returned from the call above.
    // If not, then we could have a race condition where we check the completion of this build
    // *just* after it has been inserted into the DB, but before CI had a chance to start the
    // workflow runs. In that case, bail out and wait for a later opportunity.
    if workflow_runs.is_empty() {
        match &completion_trigger {
            Some(_) => {
                return Err(anyhow::anyhow!(
                    "No workflow runs attached to SHA {} found, even though we received workflow run completed event. GitHub returned inconsistent data?",
                    build.commit_sha
                ));
            }
            None => {
                tracing::warn!(
                    "No workflow runs attached to SHA {} found. Exiting build completion check.",
                    build.commit_sha
                );
            }
        }
        return Ok(false);
    }

    // At this point, we assume that the number of GH workflow runs is final, and after a single
    // workflow run has been completed, no other workflow runs attached to the same commit can
    // appear out of nowhere.
    assert!(!workflow_runs.is_empty());

    let has_failure = workflow_runs
        .iter()
        .any(|run| matches!(run.status, WorkflowStatus::Failure));
    // If we have a failure, then we want to immediately finish the build with a failure.
    // If we don't have any failures, then we should check if we are still waiting for some
    // workflows to finish.
    if !has_failure
        && workflow_runs
            .iter()
            .any(|run| matches!(run.status, WorkflowStatus::Pending))
    {
        // We are still waiting for some workflows to be finished.
        return Ok(false);
    }

    // Below this point, we assume that the build has completed.
    // Either all workflow runs attached to the corresponding commit SHA are completed or there
    // was at least one failure.
    let build_succeeded = !has_failure;
    let pr_num = pr.map(|pr| pr.number);

    let status = if build_succeeded {
        BuildStatus::Success
    } else {
        BuildStatus::Failure
    };
    let trigger = match build.kind {
        BuildKind::Try => {
            if !build_succeeded {
                Some(LabelTrigger::TryBuildFailed)
            } else {
                None
            }
        }
        BuildKind::Auto => Some(if build_succeeded {
            LabelTrigger::AutoBuildSucceeded
        } else {
            LabelTrigger::AutoBuildFailed
        }),
        BuildKind::TryPerf => None,
    };

    let compute_duration = || {
        // Compute the time when the earliest workflow started, and when the latest workflow ended
        let start = workflow_runs.iter().map(|run| run.created_at).min()?;
        let end = workflow_runs
            .iter()
            .filter_map(|run| run.duration.map(|d| run.created_at + d))
            .max()?;

        // The build duration is the difference between those two
        (end - start).to_std().ok()
    };
    db.update_build(
        build.id,
        UpdateBuildParams::default()
            .status(status)
            .duration(compute_duration()),
    )
    .await?;
    if let (Some(trigger), Some(pr_num)) = (trigger, pr_num) {
        let pr = repo.client.get_pull_request(pr_num).await?;
        handle_label_trigger(repo, &pr, trigger).await?;
    }

    if let Some(check_run_id) = build.check_run_id {
        let (status, conclusion) = if build_succeeded {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Success))
        } else {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Failure))
        };

        if let Err(error) = repo
            .client
            .update_check_run(CheckRunId(check_run_id as u64), status, conclusion)
            .await
        {
            tracing::error!("Could not update check run {check_run_id}: {error:?}");
        }
    }

    // Trigger merge queue when an auto build completes
    if build.kind == BuildKind::Auto {
        merge_queue_tx.notify().await?;
    }
    // Enqueue rollup for unroll check
    if build.kind == BuildKind::TryPerf
        && let Some(rollup_number) = db.find_rollup_for_perf_build(build.id).await?
    {
        unroll_queue_tx
            .enqueue_rollup(build.repository.clone(), rollup_number)
            .await?;
    }

    let comment_opt = if build_succeeded {
        tracing::info!("Build succeeded for {:?} (kind={:?})", pr_num, build.kind);

        if build.kind == BuildKind::Try {
            Some(try_build_succeeded_comment(
                workflow_runs,
                CommitSha(build.commit_sha.clone()),
                CommitSha(build.parent.clone()),
            ))
        } else {
            // Merge queue will post the build succeeded comment
            None
        }
    } else {
        tracing::info!("Build failed for {:?} (kind={:?})", pr_num, build.kind);

        if build.kind == BuildKind::TryPerf {
            None
        } else {
            Some(build_failure_comment(repo, build, workflow_runs, completion_trigger).await)
        }
    };

    let Some(pr) = pr else {
        return Ok(true);
    };

    let tag = match build.kind {
        BuildKind::Try => Some(CommentTag::TryBuildStarted),
        BuildKind::Auto => Some(CommentTag::AutoBuildStarted),
        BuildKind::TryPerf => None,
    };
    if let Some(tag) = tag {
        hide_tagged_comments(repo, db, pr, tag).await?;
    }

    if let Some(comment) = comment_opt {
        repo.client.post_comment(pr.number, comment, db).await?;
    }

    Ok(true)
}

async fn build_failure_comment(
    repo: &RepositoryState,
    build: &BuildModel,
    workflow_runs: Vec<crate::bors::WorkflowRun>,
    completion_trigger: Option<CompletionTrigger>,
) -> Comment {
    let mut failed_workflow_runs: Vec<FailedWorkflowRun> = vec![];
    for workflow_run in workflow_runs {
        let failed_jobs = match get_failed_jobs(repo, workflow_run.id).await {
            Ok(jobs) => jobs,
            Err(error) => {
                tracing::error!(
                    "Cannot download jobs for workflow run {}: {error:?}",
                    workflow_run.id
                );
                vec![]
            }
        };
        failed_workflow_runs.push(FailedWorkflowRun {
            workflow_run,
            failed_jobs,
        })
    }

    let error_context = completion_trigger.and_then(|t| t.error_context);
    build_failed_comment(
        repo.repository(),
        CommitSha(build.commit_sha.clone()),
        failed_workflow_runs,
        error_context,
    )
}
