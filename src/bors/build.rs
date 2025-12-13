use crate::PgDbClient;
use crate::bors::{RepositoryState, WorkflowRun};
use crate::database::{BuildModel, BuildStatus, WorkflowModel};
use crate::github::CommitSha;
use crate::github::api::client::GithubRepositoryClient;
use octocrab::models::CheckRunId;
use octocrab::models::workflows::{Conclusion, Job, Status};
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

/// How should we treat the cancellation of a build.
pub enum CancelBuildConclusion {
    /// The build ran for too long, so we cancel it because of a timeout.
    /// It will be marked as timeouted.
    Timeout,
    /// The build was explicitly cancelled by the user.
    /// It will be marked as cancelled.
    Cancel,
}

/// Attempt to cancel a pending build.
/// It also tries to cancel its pending workflows and check run status, but that has a lesser
/// priority.
pub async fn cancel_build(
    client: &GithubRepositoryClient,
    db: &PgDbClient,
    build: &BuildModel,
    conclusion: CancelBuildConclusion,
) -> Result<Vec<WorkflowModel>, CancelBuildError> {
    assert_eq!(
        build.status,
        BuildStatus::Pending,
        "Passed a non-pending build to `cancel_build`"
    );

    // This is the most important part: we need to ensure that the status of the build is switched
    // to cancelled or timeouted.
    let status = match &conclusion {
        CancelBuildConclusion::Timeout => BuildStatus::Timeouted,
        CancelBuildConclusion::Cancel => BuildStatus::Cancelled,
    };
    db.update_build_column(build, status, None)
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

    let check_run_conclusion = match conclusion {
        CancelBuildConclusion::Timeout => CheckRunConclusion::TimedOut,
        CancelBuildConclusion::Cancel => CheckRunConclusion::Cancelled,
    };
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

/// Return failed jobs from the given workflow run.
pub async fn get_failed_jobs(
    repo: &RepositoryState,
    run_id: octocrab::models::RunId,
) -> anyhow::Result<Vec<Job>> {
    let jobs = repo.client.get_jobs_for_workflow_run(run_id).await?;
    Ok(jobs
        .into_iter()
        .filter(|j| {
            j.status == Status::Failed || {
                j.status == Status::Completed
                    && matches!(
                        j.conclusion,
                        Some(Conclusion::Failure | Conclusion::Cancelled | Conclusion::TimedOut)
                    )
            }
        })
        .collect())
}

/// Load workflows for the given build both from the DB and GitHub, and consolidate their state.
pub async fn load_workflow_runs(
    repo: &RepositoryState,
    db: &PgDbClient,
    build: &BuildModel,
) -> anyhow::Result<Vec<WorkflowRun>> {
    // Load the workflow runs that we know about from the DB. We know about workflow runs for
    // which we have received a started or a completed event.
    let db_workflow_runs = db.get_workflows_for_build(build).await?;
    tracing::debug!("Workflow runs from DB: {db_workflow_runs:?}");

    // Ask GitHub about all workflow runs attached to the build commit.
    // This tells us for how many workflow runs we should wait.
    let mut workflow_runs: Vec<WorkflowRun> = repo
        .client
        .get_workflow_runs_for_commit_sha(CommitSha(build.commit_sha.clone()))
        .await?;
    tracing::debug!("Workflow runs from GitHub: {workflow_runs:?}");

    // It is possible that the GitHub state is not fully up-to-date, or that we have overridden
    // it somehow in our DB (for example with the min_ci time mechanism).
    // It is also possible that we learn here about new GitHub workflow state that hasn't been
    // propagated to the DB yet.
    // Here we reconcile the two world views.
    for db_run in db_workflow_runs {
        if let Some(gh_run) = workflow_runs
            .iter_mut()
            .find(|gh_run| gh_run.id == db_run.run_id.into())
        {
            if gh_run.status != db_run.status && !db_run.status.is_pending() {
                // If our DB has a conclusion for the workflow that does not match GH state, we
                // override GH state with DB state, because we could have stored something special
                // in the DB, e.g. because of min_ci time check failing.
                gh_run.status = db_run.status;
            }
        } else {
            // For some reason, we have a workflow in the DB that is not on GitHub. This shouldn't
            // really happen, but in any case we backfill it.
            tracing::warn!(
                "Found DB workflow {} with status {:?} that was not on GitHub",
                db_run.run_id,
                db_run.status
            );
            workflow_runs.push(WorkflowRun {
                id: db_run.run_id.into(),
                name: db_run.name,
                url: db_run.url,
                status: db_run.status,
            });
        }
    }
    Ok(workflow_runs)
}
