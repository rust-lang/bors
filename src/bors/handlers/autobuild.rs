use std::fmt::Write;
use std::sync::Arc;

use crate::PgDbClient;
use crate::bors::comment::no_auto_build_in_progress_comment;
use crate::bors::handlers::workflow::{AutoBuildCancelReason, maybe_cancel_auto_build};
use crate::bors::handlers::{PullRequestData, deny_request, has_permission};
use crate::bors::labels::handle_label_trigger;
use crate::bors::merge_queue::{MergeQueueSender, get_pr_at_front_of_merge_queue};
use crate::bors::{CommandPrefix, Comment, RepositoryState};
use crate::database::{BuildStatus, QueueStatus};
use crate::github::{GithubUser, LabelTrigger, PullRequestNumber};
use crate::permissions::PermissionType;

pub(super) async fn command_retry(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    merge_queue_tx: &MergeQueueSender,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }

    let pr_model = pr.db;
    if matches!(pr_model.queue_status(), QueueStatus::Failed(_, _)) {
        db.clear_auto_build(pr_model).await?;
        merge_queue_tx.notify().await?;

        // Retrying is essentially like a reapproval
        handle_label_trigger(&repo_state, pr.github, LabelTrigger::Approved).await?;
    } else {
        let pending_auto_build = pr_model
            .auto_build
            .as_ref()
            .map(|b| b.status == BuildStatus::Pending)
            .unwrap_or(false);
        notify_of_invalid_retry_state(&repo_state, pr.number(), pending_auto_build, bot_prefix)
            .await?;
    }

    Ok(())
}

pub(super) async fn command_cancel(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    bot_prefix: &CommandPrefix,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }

    let auto_build_cancel_message = maybe_cancel_auto_build(
        &repo_state.client,
        &db,
        pr.db,
        AutoBuildCancelReason::Cancel,
    )
    .await?;

    let comment = match auto_build_cancel_message {
        Some(mut message) => {
            tracing::info!("Cancelled auto build");

            let next_pr = get_pr_at_front_of_merge_queue(&repo_state, &db)
                .await
                .ok()
                .flatten();
            if let Some(next_pr) = next_pr {
                writeln!(
                    message,
                    "\n\nThe next pull request likely to be tested is https://github.com/{}/pull/{}.",
                    repo_state.repository(),
                    next_pr.number
                )
                .unwrap();
            }

            merge_queue_tx.notify().await?;
            Comment::new(message)
        }
        None => {
            tracing::info!("No auto build found when trying to cancel an auto build");
            let has_pending_try_build = pr
                .db
                .try_build
                .as_ref()
                .map(|build| build.status == BuildStatus::Pending)
                .unwrap_or(false);
            no_auto_build_in_progress_comment(bot_prefix, has_pending_try_build)
        }
    };
    repo_state.client.post_comment(pr.number(), comment).await?;
    Ok(())
}

async fn notify_of_invalid_retry_state(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    running_auto_build: bool,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    let mut msg = ":exclamation: You can only retry pull requests that are approved and have a previously failed auto build.".to_string();
    if running_auto_build {
        writeln!(msg, "\n\n*Hint*: There is currently a pending auto build on this PR. To cancel it, run `{bot_prefix} cancel`.").unwrap();
    }

    repo.client
        .post_comment(pr_number, Comment::new(msg))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, Comment, GitHub, User, run_test};

    #[sqlx::test]
    async fn retry_command_insufficient_privileges(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment(Comment::from("@bors retry").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in review users"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn retry_invalid_state_error(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors retry").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":exclamation: You can only retry pull requests that are approved and have a previously failed auto build."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn retry_invalid_state_error_pending_build_hint(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            ctx.post_comment("@bors retry").await?;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @"
            :exclamation: You can only retry pull requests that are approved and have a previously failed auto build.

            *Hint*: There is currently a pending auto build on this PR. To cancel it, run `@bors cancel`.
            "
            );
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn retry_auto_build(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.workflow_full_failure(ctx.auto_workflow()).await?;
            ctx.expect_comments((), 1).await;
            ctx.post_comment("@bors retry").await?;
            ctx.wait_for_pr((), |pr| pr.auto_build.is_none()).await?;
            ctx.run_merge_queue_now().await;
            insta::assert_snapshot!(
                ctx.get_next_comment_text(()).await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-1-pr-1-d7d45f1f-reauthored-to-bors..."
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn retry_apply_label(pool: sqlx::PgPool) {
        let gh = GitHub::default().with_default_config(
            r#"
merge_queue_enabled = true

[labels]
approved = ["+foo"]
auto_build_failed = ["-foo"]
"#,
        );
        run_test((pool, gh), async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;
            ctx.workflow_full_failure(ctx.auto_workflow()).await?;
            ctx.expect_comments((), 1).await;

            ctx.post_comment("@bors retry").await?;
            ctx.pr(()).await.expect_labels(&["foo"]);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_no_running_build(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors cancel").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @":exclamation: There is currently no auto build in progress on this PR.");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn cancel_while_try_build_is_running(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors try").await?;
            ctx.expect_comments((), 1).await;
            ctx.post_comment("@bors cancel").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            :exclamation: There is currently no auto build in progress on this PR.

            *Hint*: There is a pending try build on this PR. Maybe you meant to cancel it? You can do that using `@bors try cancel`.
            ");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn cancel_cancel_workflows(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            let w1 = ctx.auto_workflow();
            let w2 = ctx.auto_workflow();
            ctx.workflow_start(w1).await?;
            ctx.workflow_start(w2).await?;
            ctx.post_comment("@bors cancel").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            Auto build cancelled. Cancelled workflows:

            - https://github.com/rust-lang/borstest/actions/runs/1
            - https://github.com/rust-lang/borstest/actions/runs/2

            The next pull request likely to be tested is https://github.com/rust-lang/borstest/pull/1.
            ");
            ctx.expect_cancelled_workflows((), &[w1, w2]);
            ctx.pr(()).await.expect_auto_build_cancelled();

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn cancel_error(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.approve(()).await?;
            ctx.start_auto_build(()).await?;

            ctx.modify_repo((), |repo| repo.workflow_cancel_error = true);
            ctx.workflow_start(ctx.auto_workflow()).await?;
            ctx.post_comment("@bors cancel").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            Auto build cancelled. It was not possible to cancel some workflows.

            The next pull request likely to be tested is https://github.com/rust-lang/borstest/pull/1.
            ");
            Ok(())
        })
        .await;
    }
}
