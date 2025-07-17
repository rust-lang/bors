use std::sync::Arc;
use std::time::Duration;

use octocrab::params::checks::CheckRunConclusion;
use octocrab::params::checks::CheckRunStatus;

use crate::PgDbClient;
use crate::bors::CheckSuiteStatus;
use crate::bors::RepositoryState;
use crate::bors::comment::{try_build_succeeded_comment, workflow_failed_comment};
use crate::bors::event::{CheckSuiteCompleted, WorkflowCompleted, WorkflowStarted};
use crate::bors::handlers::TRY_BRANCH_NAME;
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::merge_queue::AUTO_BRANCH_NAME;
use crate::bors::merge_queue::MergeQueueSender;
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
    mut payload: WorkflowCompleted,
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

    // Try to complete the build
    let event = CheckSuiteCompleted {
        repository: payload.repository,
        branch: payload.branch,
        commit_sha: payload.commit_sha,
    };
    try_complete_build(repo.as_ref(), db.as_ref(), event, merge_queue_tx).await
}

pub(super) async fn handle_check_suite_completed(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: CheckSuiteCompleted,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !is_bors_observed_branch(&payload.branch) {
        return Ok(());
    }

    tracing::info!(
        "Received check suite completed (branch={}, commit={})",
        payload.branch,
        payload.commit_sha
    );
    try_complete_build(repo.as_ref(), db.as_ref(), payload, merge_queue_tx).await
}

/// Try to complete a pending build.
async fn try_complete_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    payload: CheckSuiteCompleted,
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
        tracing::debug!("Some check suites are still pending: {checks:?}");
        return Ok(());
    }

    let has_failure = checks
        .iter()
        .any(|check| matches!(check.status, CheckSuiteStatus::Failure));
    let build_succeeded = !has_failure;

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

    let pr_num = pr.number;

    if build_succeeded {
        tracing::info!("Build succeeded for PR {pr_num}");
    } else {
        tracing::info!("Build failed for PR {pr_num}");
    }

    let branch = payload.branch.as_str();
    let (status, trigger, comment_opt) = match (branch, build_succeeded) {
        (TRY_BRANCH_NAME, true) => (
            BuildStatus::Success,
            LabelTrigger::TryBuildSucceeded,
            Some(try_build_succeeded_comment(
                &workflows,
                payload.commit_sha.clone(),
                &build,
            )),
        ),
        (TRY_BRANCH_NAME, false) => (
            BuildStatus::Failure,
            LabelTrigger::TryBuildFailed,
            Some(workflow_failed_comment(&workflows)),
        ),
        (AUTO_BRANCH_NAME, true) => (BuildStatus::Success, LabelTrigger::Succeeded, None),
        (AUTO_BRANCH_NAME, false) => (
            BuildStatus::Failure,
            LabelTrigger::Failed,
            Some(workflow_failed_comment(&workflows)),
        ),
        _ => unreachable!("Branch should be bors observed branch"),
    };

    db.update_build_status(&build, status).await?;
    handle_label_trigger(repo, pr_num, trigger).await?;

    if let Some(comment) = comment_opt {
        repo.client.post_comment(pr_num, comment).await?;
    }

    if let Some(check_run_id) = build.check_run_id {
        tracing::info!(
            "Updating check run {check_run_id} for build {} on {branch}",
            build.commit_sha,
        );
        let conclusion = if build_succeeded {
            Some(CheckRunConclusion::Success)
        } else {
            Some(CheckRunConclusion::Failure)
        };

        if let Err(error) = repo
            .client
            .update_check_run(
                check_run_id as u64,
                CheckRunStatus::Completed,
                conclusion,
                None,
            )
            .await
        {
            tracing::error!("Could not update check run {check_run_id}: {error:?}");
        }
    } else {
        tracing::warn!(
            "No check_run_id found for build {} on {branch}",
            build.commit_sha,
        );
    }

    // Trigger merge queue when an auto build completes
    if branch == AUTO_BRANCH_NAME {
        merge_queue_tx.trigger().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

    use crate::bors::handlers::trybuild::TRY_BRANCH_NAME;
    use crate::bors::merge_queue::{AUTO_BRANCH_NAME, AUTO_BUILD_CHECK_RUN_NAME};
    use crate::database::operations::get_all_workflows;
    use crate::database::{BuildStatus, WorkflowStatus};
    use crate::tests::mocks::{
        BorsBuilder, Branch, CheckSuite, GitHubState, Workflow, WorkflowEvent, default_pr_number,
        run_test,
    };

    fn gh_state_with_merge_queue() -> GitHubState {
        GitHubState::default().with_default_config(
            r#"
    merge_queue_enabled = true
    "#,
        )
    }

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

    #[sqlx::test]
    async fn auto_build_success_comment(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
merge_queue_enabled = true
"#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(|mut tester| async {
                tester.create_branch(AUTO_BRANCH_NAME).expect_suites(1);

                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;

                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                tester.workflow_success(tester.auto_branch()).await?;
                tester.process_merge_queue().await;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
            :sunny: Test successful - [Workflow1](https://github.com/workflows/Workflow1/1)
            Approved by: `default-user`
            Pushing merge-main-sha1-pr-1-sha-0 to `main`...
            "
                );

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_check_run_success(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
merge_queue_enabled = true
"#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(|mut tester| async {
                tester.create_branch(AUTO_BRANCH_NAME).expect_suites(1);
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                tester.workflow_success(tester.auto_branch()).await?;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;
                tester.expect_check_run(
                    &tester.default_pr().await.get_gh_pr().head_sha,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Success),
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_success_labels(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
merge_queue_enabled = true

[labels]
succeeded = ["+foo", "+bar", "-baz"]
"#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(|mut tester| async {
                tester.create_branch(AUTO_BRANCH_NAME).expect_suites(1);
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                let repo = tester.default_repo();
                repo.lock()
                    .get_pr(default_pr_number())
                    .check_added_labels(&[]);
                tester.workflow_success(tester.auto_branch()).await?;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                let pr = repo.lock().get_pr(default_pr_number()).clone();
                pr.check_added_labels(&["foo", "bar"]);
                pr.check_removed_labels(&["baz"]);

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failed_labels(pool: sqlx::PgPool) {
        let github = GitHubState::default().with_default_config(
            r#"
    merge_queue_enabled = true

    [labels]
    failed = ["+foo", "+bar", "-baz"]
    "#,
        );

        BorsBuilder::new(pool)
            .github(github)
            .run_test(|mut tester| async {
                tester.create_branch(AUTO_BRANCH_NAME).expect_suites(1);
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                let repo = tester.default_repo();
                repo.lock()
                    .get_pr(default_pr_number())
                    .check_added_labels(&[]);

                tester.workflow_failure(tester.auto_branch()).await?;
                tester
                    .wait_for_default_pr(|pr| {
                        pr.auto_build.as_ref().unwrap().status == BuildStatus::Failure
                    })
                    .await?;
                tester.expect_comments(1).await;

                let pr = repo.lock().get_pr(default_pr_number()).clone();
                pr.check_added_labels(&["foo", "bar"]);
                pr.check_removed_labels(&["baz"]);

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn auto_build_failure_updates_check_run_on(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(gh_state_with_merge_queue())
            .run_test(|mut tester| async {
                tester.create_branch(AUTO_BRANCH_NAME).expect_suites(1);
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;
                tester.process_merge_queue().await;
                tester.expect_comments(1).await;

                tester.workflow_failure(tester.auto_branch()).await?;
                tester.expect_comments(1).await;
                tester
                    .wait_for_default_pr(|pr| {
                        pr.auto_build.as_ref().unwrap().status == BuildStatus::Failure
                    })
                    .await?;
                tester.expect_check_run(
                    &tester.default_pr().await.get_gh_pr().head_sha,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    AUTO_BUILD_CHECK_RUN_NAME,
                    CheckRunStatus::Completed,
                    Some(CheckRunConclusion::Failure),
                );
                Ok(tester)
            })
            .await;
    }
}
