use crate::bors::RepositoryState;
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::MergeQueueSender;
use crate::database::WorkflowStatus;
use crate::github::api::client::WorkflowSource;
use crate::github::{LabelTrigger, PullRequest};

/// Note that can be attached to an approval comment.
pub enum ApprovalNote {
    /// The pull request was force approved while its PR CI is failing.
    PrCiIsFailing,
    /// The PR was approved tentatively.
    TentativeApproval,
}

/// Perform post-approve actions.
/// Should only be called if the PR is fully approved!
///
/// Notifies the merge queue and applies approval labels.
pub(super) async fn finalize_approval(
    repo: &RepositoryState,
    pr: &PullRequest,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    merge_queue_tx.notify().await?;
    handle_label_trigger(repo, &pr.clone().into(), LabelTrigger::Approved).await
}

#[derive(Copy, Clone)]
pub enum PrCiStatus {
    /// Waiting for PR CI to finish.
    Pending,
    /// PR CI has finished successfully.
    Success,
    /// PR CI has failed.
    Failed,
}

pub async fn get_pr_ci_status(
    repo: &RepositoryState,
    pr: &PullRequest,
) -> anyhow::Result<PrCiStatus> {
    let workflow_runs = repo
        .client
        .get_workflow_runs_for_commit_sha(WorkflowSource::PullRequest(pr))
        .await?;

    if workflow_runs.is_empty() {
        return Ok(PrCiStatus::Pending);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Failure)
    {
        return Ok(PrCiStatus::Failed);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Pending)
    {
        Ok(PrCiStatus::Pending)
    } else {
        Ok(PrCiStatus::Success)
    }
}
