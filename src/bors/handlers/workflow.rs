use super::trybuild::TRY_BRANCH_NAME;
use crate::PgDbClient;
use crate::bors::comment::{
    CommentTag, append_workflow_links_to_comment, build_failed_comment, try_build_succeeded_comment,
};
use crate::bors::event::{WorkflowRunCompleted, WorkflowRunStarted};
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::handlers::{BuildKind, is_bors_observed_branch};
use crate::bors::handlers::{get_build_kind, hide_try_build_started_comments};
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{FailedWorkflowRun, RepositoryState, WorkflowRun};
use crate::database::{
    BuildModel, BuildStatus, PullRequestModel, QueueStatus, WorkflowModel, WorkflowStatus,
};
use crate::github::api::client::GithubRepositoryClient;
use crate::github::{CommitSha, LabelTrigger};
use octocrab::models::CheckRunId;
use octocrab::models::workflows::{Conclusion, Job, Status};
use octocrab::params::checks::CheckRunConclusion;
use octocrab::params::checks::CheckRunStatus;
use std::sync::Arc;
use std::time::Duration;

pub(super) async fn handle_workflow_started(
    repo: Arc<RepositoryState>,
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
        payload.name.clone(),
        payload.url.clone(),
        payload.run_id.into(),
        payload.workflow_type.clone(),
        WorkflowStatus::Pending,
    )
    .await?;

    if build.branch == TRY_BRANCH_NAME {
        add_workflow_links_to_try_build_start_comment(repo, db, &build, payload).await?;
    }

    Ok(())
}

async fn add_workflow_links_to_try_build_start_comment(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    build: &BuildModel,
    payload: WorkflowRunStarted,
) -> anyhow::Result<()> {
    let Some(pr) = db.find_pr_by_build(build).await? else {
        tracing::warn!("PR for build not found");
        return Ok(());
    };
    let comments = db
        .get_tagged_bot_comments(&payload.repository, pr.number, CommentTag::TryBuildStarted)
        .await?;

    let Some(try_build_comment) = comments.last() else {
        tracing::warn!("No try build comment found for PR");
        return Ok(());
    };

    let workflows = db.get_workflow_urls_for_build(build).await?;

    if !workflows.is_empty() {
        let mut comment_content = repo
            .client
            .get_comment_content(&try_build_comment.node_id)
            .await?;

        append_workflow_links_to_comment(&mut comment_content, workflows);

        repo.client
            .update_comment_content(&try_build_comment.node_id, &comment_content)
            .await?;
    }

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

    let mut error_context = None;
    if let Some(running_time) = payload.running_time {
        let running_time =
            chrono::Duration::to_std(&running_time).unwrap_or(Duration::from_secs(0));
        if let Some(min_ci_time) = repo.config.load().min_ci_time
            && running_time < min_ci_time
        {
            tracing::warn!(
                "Workflow running time is less than the minimum CI duration: workflow time ({}s) < min time ({}s). Marking it as a failure",
                running_time.as_secs_f64(),
                min_ci_time.as_secs_f64()
            );
            payload.status = WorkflowStatus::Failure;
            error_context = Some(format!(
                "A workflow was considered to be a failure because it took only `{}s`. The minimum duration for CI workflows is configured to be `{}s`.",
                running_time.as_secs_f64(),
                min_ci_time.as_secs_f64()
            ))
        }
    }

    tracing::info!("Updating status of workflow to {:?}", payload.status);
    db.update_workflow_status(*payload.run_id, payload.status)
        .await?;

    maybe_complete_build(
        repo.as_ref(),
        db.as_ref(),
        payload,
        merge_queue_tx,
        error_context,
    )
    .await
}
/// Attempt to complete a pending build after a workflow run has been completed.
/// We assume that the status of the completed workflow run has already been updated in the
/// database.
/// We also assume that there is only a single check suite attached to a single build of a commit.
///
/// `error_context` is an additional message that should be added to a comment if the build failed.
async fn maybe_complete_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    payload: WorkflowRunCompleted,
    merge_queue_tx: &MergeQueueSender,
    error_context: Option<String>,
) -> anyhow::Result<()> {
    let Some(build_type) = get_build_kind(&payload.branch) else {
        return Ok(());
    };

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
    let build_succeeded = !has_failure;
    let pr_num = pr.number;

    let status = if build_succeeded {
        BuildStatus::Success
    } else {
        BuildStatus::Failure
    };
    let trigger = match build_type {
        BuildKind::Try => {
            if !build_succeeded {
                Some(LabelTrigger::TryBuildFailed)
            } else {
                None
            }
        }
        BuildKind::Auto => Some(if build_succeeded {
            LabelTrigger::AutoBuildSucceeded
        } else {
            LabelTrigger::AutoBuildFailed
        }),
    };

    db.update_build_status(&build, status).await?;
    if let Some(trigger) = trigger {
        handle_label_trigger(repo, pr_num, trigger).await?;
    }

    if let Some(check_run_id) = build.check_run_id {
        let (status, conclusion) = if build_succeeded {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Success))
        } else {
            (CheckRunStatus::Completed, Some(CheckRunConclusion::Failure))
        };

        if let Err(error) = repo
            .client
            .update_check_run(CheckRunId(check_run_id as u64), status, conclusion)
            .await
        {
            tracing::error!("Could not update check run {check_run_id}: {error:?}");
        }
    }

    // Trigger merge queue when an auto build completes
    if build_type == BuildKind::Auto {
        merge_queue_tx.notify().await?;
    }

    db_workflow_runs.sort_by(|a, b| a.name.cmp(&b.name));

    let comment_opt = if build_succeeded {
        tracing::info!("Build succeeded for PR {pr_num}");

        if build_type == BuildKind::Try {
            Some(try_build_succeeded_comment(
                &db_workflow_runs,
                payload.commit_sha,
                CommitSha(build.parent.clone()),
            ))
        } else {
            // Merge queue will post the build succeeded comment
            None
        }
    } else {
        tracing::info!("Build failed for PR {pr_num}");

        // Download failed jobs
        let mut workflow_runs: Vec<FailedWorkflowRun> = vec![];
        for workflow_run in db_workflow_runs {
            let failed_jobs = match get_failed_jobs(repo, &workflow_run).await {
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

        Some(build_failed_comment(
            repo.repository(),
            payload.commit_sha,
            workflow_runs,
            error_context,
        ))
    };

    if build_type == BuildKind::Try {
        hide_try_build_started_comments(repo, db, &pr).await?;
    }

    if let Some(comment) = comment_opt {
        repo.client.post_comment(pr_num, comment).await?;
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

/// Why did we cancel an auto build?
pub enum AutoBuildCancelReason {
    /// A new commit was pushed to a PR while it was being tested in an auto build.
    PushToPR,
    /// A PR was unapproved while it was being tested in an auto build.
    Unapproval,
}

/// Cancel an auto build attached to the PR, if there is any.
/// Returns an optional string that can be attached to a PR comment, which describes the result of
/// the workflow cancellation.
pub async fn maybe_cancel_auto_build(
    client: &GithubRepositoryClient,
    db: &PgDbClient,
    pr: &PullRequestModel,
    reason: AutoBuildCancelReason,
) -> anyhow::Result<Option<String>> {
    let auto_build = match pr.queue_status() {
        QueueStatus::Pending(_, build) => build,
        _ => return Ok(None),
    };

    tracing::info!("Cancelling auto build {auto_build:?}");

    match cancel_build(client, db, &auto_build, CheckRunConclusion::Cancelled).await {
        Ok(workflows) => {
            tracing::info!("Auto build cancelled");
            let workflow_urls = workflows.into_iter().map(|w| w.url).collect();
            Ok(Some(auto_build_cancelled_msg(reason, Some(workflow_urls))))
        }
        Err(CancelBuildError::FailedToMarkBuildAsCancelled(error)) => Err(error),
        Err(CancelBuildError::FailedToCancelWorkflows(error)) => {
            tracing::error!(
                "Could not cancel workflows for auto build with SHA {}: {error:?}",
                auto_build.commit_sha
            );
            Ok(Some(auto_build_cancelled_msg(reason, None)))
        }
    }
}

/// If `workflow_urls` is `None`, it was not possible to cancel workflows.
fn auto_build_cancelled_msg(
    reason: AutoBuildCancelReason,
    cancelled_workflow_urls: Option<Vec<String>>,
) -> String {
    use std::fmt::Write;

    let reason = match reason {
        AutoBuildCancelReason::PushToPR => "push",
        AutoBuildCancelReason::Unapproval => "unapproval",
    };
    let mut comment = format!("Auto build cancelled due to {reason}.");
    match cancelled_workflow_urls {
        Some(workflow_urls) => {
            comment.push_str(" Cancelled workflows:\n");
            for url in workflow_urls {
                write!(comment, "\n- {url}").unwrap();
            }
        }
        None => comment.push_str(" It was not possible to cancel some workflows."),
    }

    comment
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::database::WorkflowStatus;
    use crate::database::operations::get_all_workflows;
    use crate::tests::{BorsBuilder, BorsTester, GitHubState, default_repo_name};
    use crate::tests::{Branch, WorkflowEvent, WorkflowRunData, run_test};

    #[sqlx::test]
    async fn workflow_started_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester: &mut BorsTester| {
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
        run_test(pool.clone(), async |tester: &mut BorsTester| {
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
        run_test(pool.clone(), async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;
            tester
                .workflow_event(WorkflowEvent::started(tester.try_branch().await))
                .await?;
            Ok(())
        })
        .await;
        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn try_workflow_start_twice(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            let workflow = WorkflowRunData::from(tester.try_branch().await);
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
            tester.expect_comments((), 1).await;

            let w1 = WorkflowRunData::from(tester.try_branch().await).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch().await).with_run_id(2);

            // Let the GH mock know about the existence of the second workflow
            tester
                .modify_repo(&default_repo_name(), |repo| {
                    repo.update_workflow_run(w2.clone(), WorkflowStatus::Pending)
                })
                .await;

            // Finish w1 while w2 is not yet in the DB
            tester.workflow_full_success(w1).await?;
            tester.workflow_full_success(w2).await?;

            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r#"
            :sunny: Try build successful
            - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1) :white_check_mark:
            - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/2) :white_check_mark:
            Build commit: merge-0-pr-1 (`merge-0-pr-1`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1"} -->
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
            tester.expect_comments((), 1).await;

            let w1 = WorkflowRunData::from(tester.try_branch().await).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch().await).with_run_id(2);
            tester.workflow_start(w1.clone()).await?;
            tester.workflow_start(w2.clone()).await?;

            tester.workflow_event(WorkflowEvent::success(w1)).await?;
            tester.workflow_event(WorkflowEvent::success(w2)).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r#"
            :sunny: Try build successful
            - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1) :white_check_mark:
            - [Workflow1](https://github.com/rust-lang/borstest/actions/runs/2) :white_check_mark:
            Build commit: merge-0-pr-1 (`merge-0-pr-1`, parent: `main-sha1`)

            <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1"} -->
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
            tester.expect_comments((), 1).await;

            let w1 = WorkflowRunData::from(tester.try_branch().await).with_run_id(1);
            let w2 = WorkflowRunData::from(tester.try_branch().await).with_run_id(2);
            tester.workflow_start(w1.clone()).await?;
            tester.workflow_start(w2.clone()).await?;

            tester.workflow_event(WorkflowEvent::failure(w2)).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1), [Workflow1](https://github.com/rust-lang/borstest/actions/runs/2)"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn min_ci_time_mark_too_short_workflow_as_failed(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::default().with_default_config(
                r#"
min_ci_time = 10
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;

                // Too short workflow, it should be marked as a failure
                tester
                    .workflow_full_success(
                        WorkflowRunData::from(tester.try_branch().await)
                            .with_duration(Duration::from_secs(1)),
                    )
                    .await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
                :broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1)
                A workflow was considered to be a failure because it took only `1s`. The minimum duration for CI workflows is configured to be `10s`.
                ");
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn min_ci_time_ignore_long_enough_workflow(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::default().with_default_config(
                r#"
min_ci_time = 20
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;

                tester
                    .workflow_full_success(
                        WorkflowRunData::from(tester.try_branch().await)
                            .with_duration(Duration::from_secs(100)),
                    )
                    .await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r#"
                :sunny: Try build successful ([Workflow1](https://github.com/rust-lang/borstest/actions/runs/1))
                Build commit: merge-0-pr-1 (`merge-0-pr-1`, parent: `main-sha1`)

                <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1"} -->
                "#);
                Ok(())
            })
            .await;
    }
}
