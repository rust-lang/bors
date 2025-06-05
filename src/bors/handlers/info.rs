use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::bors::handlers::PullRequestData;
use crate::database::PgDbClient;
use crate::database::{ApprovalStatus, MergeableState};
use std::sync::Arc;

pub(super) async fn command_info(
    repo: Arc<RepositoryState>,
    pr: &PullRequestData,
    db: Arc<PgDbClient>,
) -> anyhow::Result<()> {
    use std::fmt::Write;

    let mut message = format!("## Status of PR `{}`\n", pr.number());

    // Approval info
    if let ApprovalStatus::Approved(info) = &pr.db.approval_status {
        writeln!(message, "- Approved by: `{}`", info.approver)?;
    } else {
        writeln!(message, "- Not Approved")?;
    }

    // Priority info
    if let Some(priority) = pr.db.priority {
        writeln!(message, "- Priority: {priority}")?;
    } else {
        writeln!(message, "- Priority: unset")?;
    }

    // Mergeability state
    writeln!(
        message,
        "- Mergeable: {}",
        match pr.db.mergeable_state {
            MergeableState::Mergeable => "yes",
            MergeableState::HasConflicts => "no",
            MergeableState::Unknown => "unknown",
        }
    )?;

    // Try build status
    if let Some(try_build) = &pr.db.try_build {
        writeln!(message, "- Try build is in progress")?;

        if let Ok(urls) = db.get_workflow_urls_for_build(&try_build).await {
            message.extend(
                urls.into_iter()
                    .map(|url| format!("\t- Workflow URL: {url}")),
            );
        }
    }

    repo.client
        .post_comment(pr.number(), Comment::new(message))
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::mocks::{Workflow, WorkflowEvent, run_test};

    #[sqlx::test]
    async fn info_for_unapproved_pr(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: unset
            - Mergeable: yes
            "
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_approved_pr(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            ## Status of PR `1`
            - Approved by: `default-user`
            - Priority: unset
            - Mergeable: yes
            "
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors p=5").await?;
            tester
                .wait_for_default_pr(|pr| pr.priority == Some(5))
                .await?;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: 5
            - Mergeable: yes
            "
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_try_build(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            ## Status of PR `1`
            - Not Approved
            - Priority: unset
            - Mergeable: yes
            - Try build is in progress
            "
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_everything(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments(1).await;

            tester.post_comment("@bors try").await?;
            tester.expect_comments(1).await;

            tester
                .workflow_event(WorkflowEvent::started(Workflow::from(tester.try_branch())))
                .await?;

            tester.post_comment("@bors info").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @r"
            ## Status of PR `1`
            - Approved by: `default-user`
            - Priority: 10
            - Mergeable: yes
            - Try build is in progress
            	- Workflow URL: https://github.com/workflows/Workflow1/1
            "
            );
            Ok(tester)
        })
        .await;
    }
}
