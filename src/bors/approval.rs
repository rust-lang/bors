use crate::bors::{RepositoryState, WorkflowRun};
use crate::database::WorkflowStatus;
use crate::github::PullRequest;
use crate::github::api::client::WorkflowSource;
use chrono::Utc;
use octocrab::models::WorkflowId;
use std::collections::HashMap;

/// Note that can be attached to an approval comment.
pub enum ApprovalNote {
    /// The pull request was force approved while its PR CI is failing.
    PrCiIsFailing,
    /// The PR was approved tentatively.
    TentativeApproval,
}

#[derive(Copy, Clone)]
pub enum PrCiStatus {
    /// No PR CI workflows were started yet.
    NotStarted,
    /// Waiting for PR CI workflow to finish.
    Pending { started_at: chrono::DateTime<Utc> },
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

    // PR CI, unlike auto/try CI, is sometimes retried, either manually by a user, or when the
    // PR is reopened.
    // If a workflow fails once, but then succeeds again, we should treat that as a success, and not
    // a failure. We thus keep only the latest run attempts for each workflow that we get.
    // When the workflow is retried manually, it should have an incremented `run_attempt`.
    // When the PR CI is reopened, GitHub will just create a new workflow.

    // Aggregate runs based on their workflow ID (i.e. the workflow being executed)
    // We do this via workflow ID, rather than just workflow name, though that would likely work
    // just as fine for our use-cases.
    let mut deduplicated: HashMap<WorkflowId, Vec<WorkflowRun>> = HashMap::new();
    for run in workflow_runs {
        deduplicated.entry(run.workflow_id).or_default().push(run);
    }

    // We want to keep only the latest run per workflow. Ideally, we would do this by looking at
    // the run attempt, but as noted above, this does not always work. So we simply order them by
    // creation date and then take the latest one.
    for runs in deduplicated.values_mut() {
        runs.sort_by_key(|run| run.created_at);
    }
    let mut workflow_runs = deduplicated
        .into_values()
        .filter_map(|mut runs| runs.pop())
        .collect::<Vec<WorkflowRun>>();

    if workflow_runs.is_empty() {
        return Ok(PrCiStatus::NotStarted);
    }

    if workflow_runs
        .iter()
        .any(|run| run.status == WorkflowStatus::Failure)
    {
        return Ok(PrCiStatus::Failed);
    }

    // Sort from oldest to newest, to return the oldest pending workflow
    workflow_runs.sort_by_key(|run| run.created_at);

    if let Some(pending) = workflow_runs
        .iter()
        .find(|run| run.status == WorkflowStatus::Pending)
    {
        Ok(PrCiStatus::Pending {
            started_at: pending.created_at,
        })
    } else {
        Ok(PrCiStatus::Success)
    }
}
