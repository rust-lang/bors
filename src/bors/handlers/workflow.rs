use std::sync::Arc;
use std::time::Duration;

use octocrab::models::StatusState;
use tokio::sync::mpsc;

use crate::PgDbClient;
use crate::bors::CheckSuiteStatus;
use crate::bors::PullRequestStatus;
use crate::bors::RepositoryState;
use crate::bors::comment::auto_build_failed_comment;
use crate::bors::comment::auto_build_succeeded_comment;
use crate::bors::comment::{try_build_succeeded_comment, workflow_failed_comment};
use crate::bors::event::{CheckSuiteCompleted, WorkflowCompleted, WorkflowStarted};
use crate::bors::handlers::TRY_BRANCH_NAME;
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::merge_queue::AUTO_BRANCH_NAME;
use crate::bors::merge_queue::MergeQueueEvent;
use crate::database::BuildModel;
use crate::database::PullRequestModel;
use crate::database::WorkflowModel;
use crate::database::{BuildStatus, WorkflowStatus};
use crate::github::LabelTrigger;

pub(super) async fn handle_workflow_started(
    db: Arc<PgDbClient>,
    payload: WorkflowStarted,
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
    merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    mut payload: WorkflowCompleted,
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

    // Try to complete the build
    let event = CheckSuiteCompleted {
        repository: payload.repository,
        branch: payload.branch,
        commit_sha: payload.commit_sha,
    };
    try_complete_build(repo.as_ref(), db.as_ref(), merge_queue_tx, event).await
}

pub(super) async fn handle_check_suite_completed(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    payload: CheckSuiteCompleted,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    tracing::info!(
        "Received check suite completed (branch={}, commit={})",
        payload.branch,
        payload.commit_sha
    );

    try_complete_build(repo.as_ref(), db.as_ref(), merge_queue_tx, payload).await
}

/// Try to complete a pending build.
async fn try_complete_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    payload: CheckSuiteCompleted,
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

    // Ask GitHub what are all the check suites attached to the given commit.
    // This tells us for how many workflows we should wait.
    let checks = repo
        .client
        .get_check_suites_for_commit(&payload.branch, &payload.commit_sha)
        .await?;

    // Some checks are still running, let's wait for the next event
    if checks
        .iter()
        .any(|check| matches!(check.status, CheckSuiteStatus::Pending))
    {
        return Ok(());
    }

    let has_failure = checks
        .iter()
        .any(|check| matches!(check.status, CheckSuiteStatus::Failure));

    let mut workflows = db.get_workflows_for_build(&build).await?;
    workflows.sort_by(|a, b| a.name.cmp(&b.name));

    // If this happens, there is a race condition in GH webhooks and we haven't received a workflow
    // finished/failed event for some workflow yet. In this case, wait for that event before
    // posting the PR comment.
    if workflows.len() < checks.len()
        || workflows
            .iter()
            .any(|w| w.status == WorkflowStatus::Pending)
    {
        tracing::warn!("All checks are finished, but some workflows are still pending");
        return Ok(());
    }

    let branch = payload.branch.as_str();
    let (status, trigger) = match (branch, has_failure) {
        (TRY_BRANCH_NAME, true) => (BuildStatus::Failure, LabelTrigger::TryBuildFailed),
        (TRY_BRANCH_NAME, false) => (BuildStatus::Success, LabelTrigger::TryBuildSucceeded),
        (AUTO_BRANCH_NAME, true) => (BuildStatus::Failure, LabelTrigger::AutoBuildFailed),
        (AUTO_BRANCH_NAME, false) => (BuildStatus::Success, LabelTrigger::AutoBuildSucceeded),
        _ => unreachable!(),
    };

    db.update_build_status(&build, status).await?;
    handle_label_trigger(repo, pr.number, trigger).await?;

    match branch {
        TRY_BRANCH_NAME => {
            complete_try_build(repo, pr, build, workflows, has_failure, payload).await?
        }
        AUTO_BRANCH_NAME => {
            complete_auto_build(
                db,
                repo,
                pr,
                build,
                workflows,
                has_failure,
                merge_queue_tx,
                payload,
            )
            .await?
        }
        _ => {}
    }

    Ok(())
}

/// Complete the try build workflow.
async fn complete_try_build(
    repo: &RepositoryState,
    pr: PullRequestModel,
    build: BuildModel,
    workflows: Vec<WorkflowModel>,
    has_failure: bool,
    payload: CheckSuiteCompleted,
) -> anyhow::Result<()> {
    let message = if !has_failure {
        tracing::info!("Workflow succeeded");
        try_build_succeeded_comment(&workflows, payload.commit_sha, &build)
    } else {
        tracing::info!("Workflow failed");
        workflow_failed_comment(&workflows)
    };
    repo.client.post_comment(pr.number, message).await?;

    Ok(())
}

/// Complete the auto build workflow.
#[allow(clippy::too_many_arguments)]
async fn complete_auto_build(
    db: &PgDbClient,
    repo: &RepositoryState,
    pr: PullRequestModel,
    build: BuildModel,
    workflows: Vec<WorkflowModel>,
    has_failure: bool,
    merge_queue_tx: mpsc::Sender<MergeQueueEvent>,
    payload: CheckSuiteCompleted,
) -> anyhow::Result<()> {
    let (build_succeeded, merge_succeeded) = if has_failure {
        tracing::info!("Auto build failed");
        (false, false)
    } else {
        // Update base branch to point to the tested commit
        match repo
            .client
            .set_branch_to_sha(&pr.base_branch, &payload.commit_sha)
            .await
        {
            Ok(()) => {
                tracing::info!("Auto build succeeded and merged");
                db.set_pr_status(&pr.repository, pr.number, PullRequestStatus::Merged)
                    .await?;
                (true, true)
            }
            Err(e) => {
                tracing::error!("Failed to push to base branch: {:?}", e);
                db.update_build_status(&build, BuildStatus::Failure).await?;
                (true, false)
            }
        }
    };

    let message = if !build_succeeded {
        auto_build_failed_comment(&workflows)
    } else if merge_succeeded {
        auto_build_succeeded_comment(
            &workflows,
            pr.approver().unwrap_or("<unknown>"),
            &payload.commit_sha,
            &pr.base_branch,
        )
    } else {
        todo!("Deal with failed branch push");
    };
    repo.client.post_comment(pr.number, message).await?;

    let (gh_status, gh_desc) = if build_succeeded && merge_succeeded {
        (StatusState::Success, "Build succeeded")
    } else {
        (StatusState::Failure, "Build failed")
    };

    let gh_pr = repo.client.get_pull_request(pr.number).await?;
    repo.client
        .create_commit_status(
            &gh_pr.head.sha,
            gh_status,
            None,
            Some(gh_desc),
            Some("bors"),
        )
        .await?;

    if let Err(err) = merge_queue_tx.send(()).await {
        tracing::error!("Failed to invoke merge queue: {err}");
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::database::WorkflowStatus;
    use crate::database::operations::get_all_workflows;
    use crate::tests::mocks::{Branch, CheckSuite, Workflow, WorkflowEvent, run_test};

    #[sqlx::test]
    async fn workflow_started_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester
                .workflow_event(WorkflowEvent::started(Branch::new(
                    "unknown",
                    "unknown-sha",
                )))
                .await?;
            Ok(tester)
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn workflow_completed_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester
                .workflow_event(WorkflowEvent::success(Branch::new(
                    "unknown",
                    "unknown-sha",
                )))
                .await?;
            Ok(tester)
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn try_workflow_started(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch()))
                .await?;
            Ok(tester)
        })
        .await;
        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn try_workflow_start_twice(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            let workflow = Workflow::from(tester.try_branch());
            tester
                .workflow_event(WorkflowEvent::started(workflow.clone()))
                .await?;
            tester
                .workflow_event(WorkflowEvent::started(workflow.with_run_id(2)))
                .await?;
            Ok(tester)
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 2);
    }

    #[sqlx::test]
    async fn try_check_suite_finished_missing_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester
                .check_suite(CheckSuite::completed(Branch::new(
                    "<branch>",
                    "<unknown-sha>",
                )))
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_success_multiple_suites(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(2);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_success(Workflow::from(tester.try_branch()).with_run_id(1))
                .await?;
            tester
                .workflow_success(Workflow::from(tester.try_branch()).with_run_id(2))
                .await?;
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
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_failure_multiple_suites(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(2);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester
                .workflow_success(Workflow::from(tester.try_branch()).with_run_id(1))
                .await?;
            tester
                .workflow_failure(Workflow::from(tester.try_branch()).with_run_id(2))
                .await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r###"
            :broken_heart: Test failed
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            - [Workflow1](https://github.com/workflows/Workflow1/2) :x:
            "###
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_suite_completed_received_before_workflow_completed(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            // Check suite completed received before the workflow has finished.
            // We should wait until workflow finished is received before posting the comment.
            let branch = tester.try_branch();
            tester
                .workflow_event(WorkflowEvent::started(branch.clone()))
                .await?;
            tester
                .check_suite(CheckSuite::completed(branch.clone()))
                .await?;
            tester
                .workflow_event(WorkflowEvent::success(branch))
                .await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r#"
            :sunny: Try build successful ([Workflow1](https://github.com/workflows/Workflow1/1))
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "#
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn try_check_suite_finished_twice(pool: sqlx::PgPool) {
        run_test(pool.clone(), |mut tester| async {
            tester.create_branch(TRY_BRANCH_NAME).expect_suites(1);
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;
            tester.workflow_success(tester.try_branch()).await?;
            tester
                .check_suite(CheckSuite::completed(tester.try_branch()))
                .await?;
            tester.expect_comments(1).await;
            Ok(tester)
        })
        .await;
    }
}
