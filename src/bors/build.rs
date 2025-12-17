use crate::PgDbClient;
use crate::database::{BuildModel, BuildStatus, WorkflowModel};
use crate::github::api::client::GithubRepositoryClient;
use octocrab::models::CheckRunId;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

/// We want to distinguish between a critical failure (a build was not marked as cancelled, in which
/// case bors should not continue with its normal logic), and a less important failure (could not
/// cancel some workflow or finish check run).
#[must_use]
pub enum CancelBuildError {
    /// It was not possible to mark the build as cancelled.
    FailedToMarkBuildAsCancelled(anyhow::Error),
    /// The build was marked as cancelled, but it was not possible to cancel external workflows
    /// and/or mark the check run status as cancelled.
    FailedToCancelWorkflows(anyhow::Error),
}

/// Attempt to cancel a pending build.
/// It also tries to cancel its pending workflows and check run status, but that has a lesser
/// priority.
pub async fn cancel_build(
    client: &GithubRepositoryClient,
    db: &PgDbClient,
    build: &BuildModel,
    check_run_conclusion: CheckRunConclusion,
) -> Result<Vec<WorkflowModel>, CancelBuildError> {
    assert_eq!(
        build.status,
        BuildStatus::Pending,
        "Passed a non-pending build to `cancel_build`"
    );

    // This is the most important part: we need to ensure that the status of the build is switched
    // to cancelled.
    db.update_build_status(build, BuildStatus::Cancelled)
        .await
        .map_err(CancelBuildError::FailedToMarkBuildAsCancelled)?;

    let pending_workflows = db
        .get_pending_workflows_for_build(build)
        .await
        .map_err(CancelBuildError::FailedToCancelWorkflows)?;
    let pending_workflow_ids: Vec<octocrab::models::RunId> = pending_workflows
        .iter()
        .map(|workflow| octocrab::models::RunId(workflow.run_id.0))
        .collect();

    tracing::info!("Cancelling workflows {:?}", pending_workflow_ids);
    client
        .cancel_workflows(&pending_workflow_ids)
        .await
        .map_err(CancelBuildError::FailedToCancelWorkflows)?;

    if let Some(check_run_id) = build.check_run_id
        && let Err(error) = client
            .update_check_run(
                CheckRunId(check_run_id as u64),
                CheckRunStatus::Completed,
                Some(check_run_conclusion),
            )
            .await
    {
        tracing::error!("Could not update check run {check_run_id} for build {build:?}: {error:?}");
    }

    Ok(pending_workflows)
}
