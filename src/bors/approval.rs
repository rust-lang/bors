use crate::bors::RepositoryState;
use crate::database::WorkflowStatus;
use crate::github::PullRequest;
use crate::github::api::client::WorkflowSource;

/// Note that can be attached to an approval comment.
pub enum ApprovalNote {
    /// The pull request was force approved while its PR CI is failing.
    PrCiIsFailing,
    /// The PR was approved tentatively.
    TentativeApproval,
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
    tracing::info!(
        "Found PR CI workflows for PR #{} and HEAD SHA {}: {workflow_runs:?}",
        pr.number,
        pr.head.sha
    );

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
