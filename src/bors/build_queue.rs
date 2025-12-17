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

use crate::bors::build::{CancelBuildError, cancel_build};
use crate::bors::comment::{
    CommentTag, build_failed_comment, build_timed_out_comment, try_build_succeeded_comment,
};
use crate::bors::event::WorkflowRunCompleted;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{BuildKind, FailedWorkflowRun, RepositoryState, WorkflowRun, elapsed_time_since};
use crate::database::{BuildModel, BuildStatus, PullRequestModel, WorkflowStatus};
use crate::github::{CommitSha, GithubRepoName, LabelTrigger};
use crate::{BorsContext, PgDbClient};
use octocrab::models::CheckRunId;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

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
                let Some(pr) = db.find_pr_by_build(&build).await? else {
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
                        "Detected orphaned pending without a PR, marking it as time outed: {build:?}"
                    );
                    db.update_build_status(&build, BuildStatus::Timeouted)
                        .await?;
                    continue;
                };

                if let Err(error) = maybe_timeout_build(&repo, db, &build, &pr, timeout).await {
                    tracing::error!("Could not timeout pending build {build:?}: {error:?}");
                }
            }
        }
        BuildQueueEvent::OnWorkflowCompleted { event, repo, .. } => {
            // let build = db
            //     .find_build(&repo, &event.branch, event.commit_sha)
            //     .await?;
            // let Some(build) = build else {
            //     return Ok(());
            // };
            // if build.status != BuildStatus::Pending {
            //     return Ok(());
            // }
            // let Some(pr) = db.find_pr_by_build(&build).await? else {
            //     return Ok(());
            // };
        }
    }

    #[cfg(test)]
    crate::bors::WAIT_FOR_BUILD_QUEUE.mark();

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
        match cancel_build(&repo.client, db, build, CheckRunConclusion::TimedOut).await {
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

        if let Err(error) = repo
            .client
            .post_comment(pr.number, build_timed_out_comment(timeout))
            .await
        {
            tracing::error!("Could not send comment to PR {}: {error:?}", pr.number);
        }
    }
    Ok(())
}
