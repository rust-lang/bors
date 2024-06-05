use std::sync::Arc;

use crate::bors::comment::try_build_succeeded_comment;
use crate::bors::event::{CheckSuiteCompleted, WorkflowCompleted, WorkflowStarted};
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::{CheckSuiteStatus, Comment};
use crate::bors::{RepositoryClient, RepositoryState};
use crate::database::{BuildStatus, DbClient, WorkflowStatus};
use crate::github::LabelTrigger;

pub(super) async fn handle_workflow_started(
    db: Arc<dyn DbClient>,
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

pub(super) async fn handle_workflow_completed<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    db: Arc<dyn DbClient>,
    payload: WorkflowCompleted,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
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
    try_complete_build(repo.as_ref(), db.as_ref(), event).await
}

pub(super) async fn handle_check_suite_completed<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    db: Arc<dyn DbClient>,
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
    try_complete_build(repo.as_ref(), db.as_ref(), payload).await
}

/// Try to complete a pending build.
async fn try_complete_build<Client: RepositoryClient>(
    repo: &RepositoryState<Client>,
    db: &dyn DbClient,
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

    let workflow_list = workflows
        .into_iter()
        .map(|w| {
            format!(
                "- [{}]({}) {}",
                w.name,
                w.url,
                if w.status == WorkflowStatus::Success {
                    ":white_check_mark:"
                } else {
                    ":x:"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let (status, trigger) = if has_failure {
        (BuildStatus::Failure, LabelTrigger::TryBuildFailed)
    } else {
        (BuildStatus::Success, LabelTrigger::TryBuildSucceeded)
    };
    db.update_build_status(&build, status).await?;

    handle_label_trigger(repo, pr.number, trigger).await?;

    let message = if !has_failure {
        tracing::info!("Workflow succeeded");

        try_build_succeeded_comment(workflow_list, payload.commit_sha)
    } else {
        tracing::info!("Workflow failed");
        Comment::new(format!(
            r#":broken_heart: Test failed
{}"#,
            workflow_list
        ))
    };
    repo.client.post_comment(pr.number, message).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::database::operations::get_all_workflows;
    use crate::database::WorkflowStatus;
    use crate::tests::mocks::{
        run_test, Branch, CheckSuite, TestWorkflowStatus, Workflow, WorkflowEvent,
    };

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
                @r###"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            - [Workflow1](https://github.com/workflows/Workflow1/2) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`)
            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "###
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
                @r###"
            :sunny: Try build successful
            - [Workflow1](https://github.com/workflows/Workflow1/1) :white_check_mark:
            Build commit: merge-main-sha1-pr-1-sha-0 (`merge-main-sha1-pr-1-sha-0`)
            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-main-sha1-pr-1-sha-0"} -->
            "###
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
