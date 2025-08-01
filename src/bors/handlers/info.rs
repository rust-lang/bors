use crate::bors::RepositoryState;
use crate::bors::comment::info_comment;
use crate::bors::handlers::PullRequestData;
use crate::database::PgDbClient;
use std::sync::Arc;

pub(super) async fn command_info(
    repo: Arc<RepositoryState>,
    pr: &PullRequestData,
    db: Arc<PgDbClient>,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr.number(), info_comment(&pr.db, db).await)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, WorkflowEvent, WorkflowRunData, run_test};

    #[sqlx::test]
    async fn info_for_unapproved_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: unset
            - Mergeable: yes
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_approved_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments((), 1).await;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            ## Status of PR `1`
            - Approved by: `default-user`
            - Priority: unset
            - Mergeable: yes
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors p=5").await?;
            tester.wait_for_pr((), |pr| pr.priority == Some(5)).await?;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: 5
            - Mergeable: yes
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_try_build(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: unset
            - Mergeable: yes
            - Try build is in progress
            "
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_everything(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments((), 1).await;

            tester.post_comment("@bors try").await?;
            tester.expect_comments((), 1).await;

            tester
                .workflow_event(WorkflowEvent::started(WorkflowRunData::from(
                    tester.try_branch().await,
                )))
                .await?;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment(()).await?,
                @r"
            ## Status of PR `1`
            - Approved by: `default-user`
            - Priority: 10
            - Mergeable: yes
            - Try build is in progress
            	- Workflow URL: https://github.com/rust-lang/borstest/actions/runs/1
            "
            );
            Ok(())
        })
        .await;
    }
}
