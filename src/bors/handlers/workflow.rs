use crate::PgDbClient;
use crate::bors::comment::{build_failed_comment, try_build_succeeded_comment};
use crate::bors::event::{WorkflowRunCompleted, WorkflowRunStarted};
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::merge_queue::AUTO_BRANCH_NAME;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{FailedWorkflowRun, RepositoryState, WorkflowRun};
use crate::database::{BuildStatus, WorkflowModel, WorkflowStatus};
use crate::github::LabelTrigger;
use octocrab::models::CheckRunId;
use octocrab::models::workflows::{Conclusion, Job, Status};
use octocrab::params::checks::CheckRunConclusion;
use octocrab::params::checks::CheckRunStatus;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn handle_workflow_started(
    db: Arc<PgDbClient>,
    payload: WorkflowRunStarted,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    tracing::info!(
        "Handling workflow started (name={}, url={}, branch={}, commit={})",
        payload.name,
        payload.url,
        payload.branch,
        payload.commit_sha
    );

    let Some(build) = db
        .find_build(
            &payload.repository,
            payload.branch.clone(),
            payload.commit_sha.clone(),
        )
        .await?
    else {
        tracing::warn!("Build for workflow not found");
        return Ok(());
    };

    // This can happen e.g. if the build is cancelled quickly
    if build.status != BuildStatus::Pending {
        tracing::warn!("Received workflow started for an already completed build");
        return Ok(());
    }

    tracing::info!("Storing workflow started into DB");
    db.create_workflow(
        &build,
        payload.name,
        payload.url,
        payload.run_id.into(),
        payload.workflow_type,
        WorkflowStatus::Pending,
    )
    .await?;

    Ok(())
}

pub(super) async fn handle_workflow_completed(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mut payload: WorkflowRunCompleted,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    if let Some(running_time) = payload.running_time {
        let running_time_as_duration =
            chrono::Duration::to_std(&running_time).unwrap_or(Duration::from_secs(0));
        if let Some(min_ci_time) = repo.config.load().min_ci_time {
            if running_time_as_duration < min_ci_time {
                payload.status = WorkflowStatus::Failure;
                tracing::warn!(
                    "Workflow running time is less than the minimum CI duration: {:?} < {:?}",
                    running_time_as_duration,
                    min_ci_time
                );
            }
        }
    } else {
        tracing::warn!("Running time is not available.");
    }

    tracing::info!("Updating status of workflow to {:?}", payload.status);
    db.update_workflow_status(*payload.run_id, payload.status)
        .await?;

    maybe_complete_build(repo.as_ref(), db.as_ref(), payload, merge_queue_tx).await
}

/// Attempt to complete a pending build after a workflow run has been completed.
/// We assume that the status of the completed workflow run has already been updated in the
/// database.
/// We also assume that there is only a single check suite attached to a single build of a commit.
async fn maybe_complete_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    payload: WorkflowRunCompleted,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    let Some(build) = db
        .find_build(
            &payload.repository,
            payload.branch.clone(),
            payload.commit_sha.clone(),
        )
        .await?
    else {
        tracing::warn!(
            "Received check suite finished for an unknown build: {}",
            payload.commit_sha
        );
        return Ok(());
    };

    // If the build has already been marked with a conclusion, ignore this event
    if build.status != BuildStatus::Pending {
        return Ok(());
    }

    let Some(pr) = db.find_pr_by_build(&build).await? else {
        tracing::warn!("Cannot find PR for build {}", build.commit_sha);
        return Ok(());
    };

    // Load the workflow runs that we know about from the DB. We know about workflow runs for
    // which we have received a started or a completed event.
    let mut db_workflow_runs = db.get_workflows_for_build(&build).await?;
    tracing::debug!("Workflow runs from DB: {db_workflow_runs:?}");

    // If the workflow run was a success, check if we're still waiting for some other workflow run.
    // If it was a failure, then immediately mark the build as failed.
    if payload.status == WorkflowStatus::Success {
        {
            // Ask GitHub about all workflow runs attached to the check suite of the completed workflow run.
            // This tells us for how many workflow runs we should wait.
            // We assume that this number is final, and after a single workflow run has been completed, no
            // other workflow runs attached to the check suite can appear out of nowhere.
            // Note: we actually only need the number of workflow runs in this function, but we download
            // some data about them to have better logging.
            let gh_workflow_runs: Vec<WorkflowRun> = repo
                .client
                .get_workflow_runs_for_check_suite(payload.check_suite_id)
                .await?;
            tracing::debug!("Workflow runs from GitHub: {gh_workflow_runs:?}");

            // This could happen if a workflow run webhook is lost, or if one workflow run manages to finish
            // before another workflow run even manages to start. It should be rare.
            // We will wait for the next workflow run completed webhook.
            if db_workflow_runs.len() < gh_workflow_runs.len() {
                tracing::warn!("Workflow count mismatch, waiting for the next webhook");
                return Ok(());
            }
        }

        // We have all expected workflow runs in the DB, but some of them are still pending.
        // Wait for the next workflow run to be finished.
        if db_workflow_runs
            .iter()
            .any(|w| w.status == WorkflowStatus::Pending)
        {
            tracing::info!("Some workflows are not finished yet, waiting for the next webhook.");
            return Ok(());
        }
    }

    // Below this point, we assume that the build has completed.
    // Either all workflow runs attached to the corresponding check suite are completed or there
    // was at least one failure.

    let has_failure = db_workflow_runs
        .iter()
        .any(|check| matches!(check.status, WorkflowStatus::Failure));

    let (status, trigger) = if has_failure {
        (BuildStatus::Failure, LabelTrigger::TryBuildFailed)
    } else {
        (BuildStatus::Success, LabelTrigger::TryBuildSucceeded)
    };
    db.update_build_status(&build, status).await?;

    handle_label_trigger(repo, pr.number, trigger).await?;

    if let Some(check_run_id) = build.check_run_id {
        let (status, conclusion) = if has_failure {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Failure))
        } else {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Success))
        };

        if let Err(error) = repo
            .client
            .update_check_run(CheckRunId(check_run_id as u64), status, conclusion, None)
            .await
        {
            tracing::error!("Could not update check run {check_run_id}: {error:?}");
        }
    }

    db_workflow_runs.sort_by(|a, b| a.name.cmp(&b.name));

    let message = if !has_failure {
        tracing::info!("Build succeeded");
        try_build_succeeded_comment(&db_workflow_runs, payload.commit_sha, &build)
    } else {
        // Download failed jobs
        let mut workflow_runs: Vec<FailedWorkflowRun> = vec![];
        for workflow_run in db_workflow_runs {
            let failed_jobs = match get_failed_jobs(&repo, &workflow_run).await {
                Ok(jobs) => jobs,
                Err(error) => {
                    tracing::error!(
                        "Cannot download jobs for workflow run {}: {error:?}",
                        workflow_run.run_id
                    );
                    vec![]
                }
            };
            workflow_runs.push(FailedWorkflowRun {
                workflow_run,
                failed_jobs,
            })
        }

        tracing::info!("Build failed");
        build_failed_comment(repo.repository(), workflow_runs)
    };
    repo.client.post_comment(pr.number, message).await?;

    // Trigger merge queue when an auto build completes
    if payload.branch == AUTO_BRANCH_NAME {
        merge_queue_tx.trigger().await?;
    }

    Ok(())
}

/// Return failed jobs from the given workflow run.
async fn get_failed_jobs(
    repo: &RepositoryState,
    workflow_run: &WorkflowModel,
) -> anyhow::Result<Vec<Job>> {
    let jobs = repo
        .client
        .get_jobs_for_workflow_run(workflow_run.run_id.into())
        .await?;
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

#[cfg(test)]
mod tests {
    use crate::database::WorkflowStatus;
    use crate::database::operations::get_all_workflows;
    use crate::tests::BorsTester;
    use crate::tests::mocks::{Branch, WorkflowEvent, WorkflowRunData, run_test};

    #[sqlx::test]
    async fn workflow_started_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester| {
            tester
                .workflow_event(WorkflowEvent::started(Branch::new(
                    "unknown",
                    "unknown-sha",
                )))
                .await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn workflow_completed_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester| {
            tester
                .workflow_event(WorkflowEvent::success(Branch::new(
                    "unknown",
                    "unknown-sha",
                )))
                .await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn try_workflow_started(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch()))
                .await?;
            Ok(())
        })
        .await;
        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn try_workflow_start_twice(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let workflow = WorkflowRunData::from(tester.try_branch());
            tester
                .workflow_event(WorkflowEvent::started(workflow.clone()))
                .await?;
            tester
                .workflow_event(WorkflowEvent::started(workflow.with_run_id(2)))
                .await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 2);
    }

    // First start both workflows, then finish both of them.
    #[sqlx::test]
    async fn try_success_multiple_workflows_per_suite_1(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let w1 = WorkflowRunData::from(tester.try_branch()).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch()).with_run_id(2);

            // Let the GH mock know about the existence of the second workflow
            tester.default_repo().lock().update_workflow_run(w2.clone(), WorkflowStatus::Pending);
            // Finish w1 while w2 is not yet in the DB
            tester.workflow_full_success(w1).await?;
            tester.workflow_full_success(w2).await?;

            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r#"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            - [Workflow1](https://github.com/workflows/Workflow1/2) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "#
            );
            Ok(())
        })
        .await;
    }

    // First start and finish the first workflow, then do the same for the second one.
    #[sqlx::test]
    async fn try_success_multiple_workflows_per_suite_2(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let w1 = WorkflowRunData::from(tester.try_branch()).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch()).with_run_id(2);
            tester.workflow_start(w1.clone()).await?;
            tester.workflow_start(w2.clone()).await?;

            tester.workflow_event(WorkflowEvent::success(w1)).await?;
            tester.workflow_event(WorkflowEvent::success(w2)).await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r#"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            - [Workflow1](https://github.com/workflows/Workflow1/2) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "#
            );
            Ok(())
        })
            .await;
    }

    // Finish the build early when we encounter the first failure
    #[sqlx::test]
    async fn try_failure_multiple_workflows_early(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let w1 = WorkflowRunData::from(tester.try_branch()).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch()).with_run_id(2);
            tester.workflow_start(w1.clone()).await?;
            tester.workflow_start(w2.clone()).await?;

            tester.workflow_event(WorkflowEvent::failure(w2)).await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @":broken_heart: Test failed ([Workflow1](https://github.com/workflows/Workflow1/1), [Workflow1](https://github.com/workflows/Workflow1/2))"
            );
            Ok(())
        })
        .await;
    }
}
