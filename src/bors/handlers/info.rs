use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::database::ApprovalStatus;
use crate::database::PgDbClient;
use crate::github::PullRequest;
use std::sync::Arc;

pub(super) async fn command_info(
    repo: Arc<RepositoryState>,
    pr: &PullRequest,
    db: Arc<PgDbClient>,
) -> anyhow::Result<()> {
    // Geting PR info from database
    let pr_model = db
        .get_or_create_pull_request(
            repo.client.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    // Building the info message
    let mut info_lines = Vec::new();

    // Approval info
    if let ApprovalStatus::Approved(info) = pr_model.approval_status {
        info_lines.push(format!("- **Approved by:** @{}", info.approver));
    } else {
        info_lines.push("- **Not Approved:**".to_string());
    }

    // Priority info
    if let Some(priority) = pr_model.priority {
        info_lines.push(format!("- **Priority:** {}", priority));
    } else {
        info_lines.push("- **Priority:** Not set".to_string());
    }

    // Build status
    if let Some(try_build) = pr_model.try_build {
        info_lines.push(format!("- **Try build branch:** {}", try_build.branch));

        if let Ok(urls) = db.get_workflow_urls_for_build(&try_build).await {
            info_lines.extend(
                urls.into_iter()
                    .map(|url| format!("- **Workflow URL:** {}", url)),
            );
        }
    }

    // Joining all lines
    let info = info_lines.join("\n");

    // Post the comment
    repo.client
        .post_comment(pr.number, Comment::new(info))
        .await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::mocks::{create_world_with_approve_config, BorsBuilder, World};

    #[sqlx::test]
    async fn info_for_unapproved_pr(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(World::default())
            .run_test(|mut tester| async {
                tester.post_comment("@bors info").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
                - **Not Approved:**
                - **Priority:** Not set
                "
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn info_for_approved_pr(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors info").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
                - **Approved by:** @default-user
                - **Priority:** Not set
                "
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_priority(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors p=5").await?;
                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
                        Ok(pr.priority == Some(5))
                    })
                    .await?;

                tester.post_comment("@bors info").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
                - **Not Approved:**
                - **Priority:** 5
                "
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_try_build(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors info").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
                - **Not Approved:**
                - **Priority:** Not set
                - **Try build branch:** automation/bors/try
                "
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn info_for_pr_with_everything(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+ p=10").await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors info").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @r"
                - **Approved by:** @default-user
                - **Priority:** 10
                - **Try build branch:** automation/bors/try
                "
                );
                Ok(tester)
            })
            .await;
    }
}
