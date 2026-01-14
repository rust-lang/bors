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
use crate::bors::{
    BuildKind, FailedWorkflowRun, RepositoryState, elapsed_time_since, hide_tagged_comments,
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
        self.inner
            .send(BuildQueueEvent::OnWorkflowCompleted {
                event,
                error_context,
            })
            .await
    }
}

pub fn create_buid_queue() -> (BuildQueueSender, BuildQueueReceiver) {
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
                    if let Some(pr) = db.find_pr_by_build(&build).await? {
                        // First try to complete builds, and only then timeout then
                        // Because if the bot was offline for some time, we want to first attempt to
                        // actually finish the build, otherwise it might get instantly timeouted.
                        if !maybe_complete_build(
                            &repo,
                            db,
                            &build,
                            &pr,
                            &merge_queue_tx,
                            None,
                            None,
                        )
                        .await?
                        {
                            maybe_timeout_build(&repo, db, &build, &pr, timeout).await?;
                        }
                    } else {
                        // This is an orphaned build. It should never be created, unless we have some bug or
                        // unexpected race condition in bors.
                        // When we do encounter such a build, we can mark it as timeouted, as it is no longer
                        // relevant.
                        // Note that we could write an explicit query for finding these orphaned builds,
                        // but that could be quite expensive. Instead we piggyback on the existing logic
                        // for timed out builds; if a build is still pending and has no PR attached, then
                        // there likely won't be any additional event that could mark it as finished.
                        // So eventually all such builds will arrive here
                        tracing::warn!(
                            "Detected orphaned pending without a PR, marking it as timeouted: {build:?}"
                        );
                        db.update_build(
                            build.id,
                            UpdateBuildParams::default().status(BuildStatus::Timeouted),
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
            let Some(pr) = db.find_pr_by_build(&build).await? else {
                return Ok(());
            };
            let repo = ctx.get_repo(&event.repository)?;
            maybe_complete_build(
                &repo,
                db,
                &build,
                &pr,
                &merge_queue_tx,
                Some(CompletionTrigger { error_context }),
                event.running_time,
            )
            .await?;
        }
    }

    Ok(())
}

async fn maybe_timeout_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
    pr: &PullRequestModel,
    timeout: Duration,
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

        // Also handle label triggers
        let trigger = match build.kind {
            BuildKind::Try => LabelTrigger::TryBuildFailed,
            BuildKind::Auto => LabelTrigger::AutoBuildFailed,
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
    pr: &PullRequestModel,
    merge_queue_tx: &MergeQueueSender,
    completion_trigger: Option<CompletionTrigger>,
    running_time: Option<chrono::Duration>,
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
    let pr_num = pr.number;

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
    };

    db.update_build(
        build.id,
        UpdateBuildParams::default()
            .status(status)
            .duration(running_time),
    )
    .await?;
    if let Some(trigger) = trigger {
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

    let comment_opt = if build_succeeded {
        tracing::info!("Build succeeded for PR {pr_num}");

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
        tracing::info!("Build failed for PR {pr_num}");

        // Download failed jobs
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
        Some(build_failed_comment(
            repo.repository(),
            CommitSha(build.commit_sha.clone()),
            failed_workflow_runs,
            error_context,
        ))
    };

    let tag = match build.kind {
        BuildKind::Try => CommentTag::TryBuildStarted,
        BuildKind::Auto => CommentTag::AutoBuildStarted,
    };
    hide_tagged_comments(repo, db, pr, tag).await?;

    if let Some(comment) = comment_opt {
        repo.client.post_comment(pr_num, comment, db).await?;
    }

    Ok(true)
}
