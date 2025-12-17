use std::sync::Arc;

use crate::PgDbClient;
use crate::bors::handlers::{PullRequestData, deny_request, has_permission};
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{Comment, RepositoryState};
use crate::database::QueueStatus;
use crate::github::{GithubUser, PullRequestNumber};
use crate::permissions::PermissionType;

pub(super) async fn command_retry(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }

    let pr_model = pr.db;

    if matches!(pr_model.queue_status(), QueueStatus::Failed(_, _)) {
        db.clear_auto_build(pr_model).await?;
        merge_queue_tx.notify().await?;
    } else {
        notify_of_invalid_retry_state(&repo_state, pr.number()).await?;
    }

    Ok(())
}

async fn notify_of_invalid_retry_state(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(":exclamation: You can only retry pull requests that are approved and have a previously failed auto build".to_string())
        )
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, Comment, User, run_test};

    #[sqlx::test]
    async fn retry_command_insufficient_privileges(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(Comment::from("@bors retry").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in review users"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn retry_invalid_state_error(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment(Comment::from("@bors retry")).await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":exclamation: You can only retry pull requests that are approved and have a previously failed auto build"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn retry_auto_build(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.workflow_full_failure(tester.auto_branch()).await?;
            tester.expect_comments((), 1).await;
            tester.post_comment(Comment::from("@bors retry")).await?;
            tester.wait_for_pr((), |pr| pr.auto_build.is_none()).await?;
            tester.process_merge_queue().await;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":hourglass: Testing commit pr-1-sha with merge merge-1-pr-1..."
            );
            Ok(())
        })
        .await;
    }
}
