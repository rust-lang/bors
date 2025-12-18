use crate::PgDbClient;
use crate::bors::RepositoryState;
use crate::bors::build::CancelBuildError;
use crate::bors::build_queue::BuildQueueSender;
use crate::bors::comment::{CommentTag, append_workflow_links_to_comment};
use crate::bors::event::{WorkflowRunCompleted, WorkflowRunStarted};
use crate::bors::handlers::is_bors_observed_branch;
use crate::bors::{BuildKind, build};
use crate::database::{BuildModel, BuildStatus, PullRequestModel, QueueStatus, WorkflowStatus};
use crate::github::api::client::GithubRepositoryClient;
use octocrab::params::checks::CheckRunConclusion;
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
            &payload.branch,
            payload.commit_sha.clone(),
        )
        .await?
    else {
        tracing::warn!("Build for workflow started not found");
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

    add_workflow_links_to_build_start_comment(repo, db, &build, payload).await?;

    Ok(())
}

async fn add_workflow_links_to_build_start_comment(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    build: &BuildModel,
    payload: WorkflowRunStarted,
) -> anyhow::Result<()> {
    let Some(pr) = db.find_pr_by_build(build).await? else {
        tracing::warn!("PR for build not found");
        return Ok(());
    };

    let tag = match build.kind {
        BuildKind::Try => CommentTag::TryBuildStarted,
        BuildKind::Auto => CommentTag::AutoBuildStarted,
    };
    let comments = db
        .get_tagged_bot_comments(&payload.repository, pr.number, tag)
        .await?;

    let Some(build_started_comment) = comments.last() else {
        tracing::warn!("No build started comment found for PR");
        return Ok(());
    };

    let workflows = db.get_workflow_urls_for_build(build).await?;
    if !workflows.is_empty() {
        let mut comment_content = repo
            .client
            .get_comment_content(&build_started_comment.node_id)
            .await?;

        append_workflow_links_to_comment(&mut comment_content, workflows);

        repo.client
            .update_comment_content(&build_started_comment.node_id, &comment_content)
            .await?;
    }

    Ok(())
}

pub(super) async fn handle_workflow_completed(
    repo: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    mut payload: WorkflowRunCompleted,
    build_queue_tx: &BuildQueueSender,
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

    build_queue_tx
        .on_workflow_completed(payload, error_context)
        .await?;
    Ok(())
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

    match build::cancel_build(client, db, &auto_build, CheckRunConclusion::Cancelled).await {
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
    use crate::tests::{BorsBuilder, BorsTester, GitHub};
    use crate::tests::{WorkflowEvent, run_test};

    #[sqlx::test]
    async fn workflow_started_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |ctx: &mut BorsTester| {
            ctx.create_branch("unknown");
            let run_id = ctx.create_workflow((), "unknown");
            ctx.workflow_event(WorkflowEvent::started(run_id)).await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn workflow_completed_unknown_build(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |ctx: &mut BorsTester| {
            ctx.create_branch("unknown");
            let run_id = ctx.create_workflow((), "unknown");
            ctx.workflow_event(WorkflowEvent::success(run_id)).await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 0);
    }

    #[sqlx::test]
    async fn try_workflow_started(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;
            ctx.workflow_event(WorkflowEvent::started(ctx.try_workflow()))
                .await?;
            Ok(())
        })
        .await;
        let suite = get_all_workflows(&pool).await.unwrap().pop().unwrap();
        assert_eq!(suite.status, WorkflowStatus::Pending);
    }

    #[sqlx::test]
    async fn try_workflow_start_twice(pool: sqlx::PgPool) {
        run_test(pool.clone(), async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;

            ctx.workflow_event(WorkflowEvent::started(ctx.try_workflow()))
                .await?;
            ctx.workflow_event(WorkflowEvent::started(ctx.try_workflow()))
                .await?;
            Ok(())
        })
        .await;
        assert_eq!(get_all_workflows(&pool).await.unwrap().len(), 2);
    }

    // First start both workflows, then finish both of them.
    #[sqlx::test]
    async fn try_success_multiple_workflows_per_suite_1(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;

            let w1 = ctx.try_workflow();
            let w2 = ctx.try_workflow();

            // Finish w1 while w2 is not yet in the DB
            ctx.workflow_full_success(w1).await?;
            ctx.workflow_full_success(w2).await?;

            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
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
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;

            let w1 = ctx.try_workflow();
            let w2 = ctx.try_workflow();
            ctx.workflow_start(w1).await?;
            ctx.workflow_start(w2).await?;

            ctx.workflow_event(WorkflowEvent::success(w1)).await?;
            ctx.workflow_event(WorkflowEvent::success(w2)).await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
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
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;

            let w1 = ctx.try_workflow();
            let w2 = ctx.try_workflow();
            ctx.workflow_start(w1).await?;
            ctx.workflow_start(w2).await?;

            ctx.workflow_event(WorkflowEvent::failure(w2)).await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":broken_heart: Test for merge-0-pr-1 failed: [Workflow1](https://github.com/rust-lang/borstest/actions/runs/1), [Workflow1](https://github.com/rust-lang/borstest/actions/runs/2)"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn min_ci_time_mark_too_short_workflow_as_failed(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
min_ci_time = 10
"#,
            ))
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;

                let w1 = ctx.try_workflow();
                ctx.modify_workflow(w1, |w| w.set_duration(Duration::from_secs(1)));

                // Too short workflow, it should be marked as a failure
                ctx
                    .workflow_full_success(w1)
                    .await?;
                insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @r"
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
            .github(GitHub::default().with_default_config(
                r#"
min_ci_time = 20
"#,
            ))
            .run_test(async |ctx: &mut BorsTester| {
                ctx.post_comment("@bors try").await?;
                ctx.expect_comments((), 1).await;

                let w1 = ctx.try_workflow();
                ctx.modify_workflow(w1, |w| w.set_duration(Duration::from_secs(100)));
                ctx
                    .workflow_full_success(w1)
                    .await?;
                insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @r#"
                :sunny: Try build successful ([Workflow1](https://github.com/rust-lang/borstest/actions/runs/1))
                Build commit: merge-0-pr-1 (`merge-0-pr-1`, parent: `main-sha1`)

                <!-- homu: {"type":"TryBuildCompleted","merge_sha":"merge-0-pr-1"} -->
                "#);
                Ok(())
            })
            .await;
    }
}
