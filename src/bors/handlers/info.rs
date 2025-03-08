use crate::bors::Comment;
use crate::bors::RepositoryState;
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
        .get_or_create_pull_request(repo.client.repository(), pr.number)
        .await?;

    // Building the info message
    let mut info_lines = Vec::new();

    // Approval info
    if let Some(approved_by) = pr_model.approved_by {
        info_lines.push(format!("- **Approved by:** @{}", approved_by));
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
        if let Ok(Some(url)) = db.get_workflow_url_for_build(&try_build).await {
            info_lines.push(format!("- **Workflow URL:** {}", url));
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
    use crate::tests::mocks::{BorsBuilder, User, World};

    fn create_world_with_approve_config() -> World {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
approve = ["+approved"]
"#,
        );
        world
    }

    #[sqlx::test]
    async fn info_for_unapproved_pr(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(World::default())
            .run_test(|mut tester| async {
                tester.post_comment("@bors info").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "- **Not Approved:**\n- **Priority:** Not set"
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
                // First approve the PR
                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;

                // Then check info
                tester.post_comment("@bors info").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "- **Approved by:** @{}\n- **Priority:** Not set",
                        User::default_user().name
                    )
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
                // Set priority
                tester.post_comment("@bors p=5").await?;
                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
                        Ok(pr.priority == Some(5))
                    })
                    .await?;

                // Check info
                tester.post_comment("@bors info").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "- **Not Approved:**\n- **Priority:** 5"
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
                // Create a try build
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                // Check info
                tester.post_comment("@bors info").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "- **Not Approved:**\n- **Priority:** Not set\n- **Try build branch:** automation/bors/try"
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
                // Approve with priority
                tester.post_comment("@bors r+ p=10").await?;
                tester.expect_comments(1).await;

                // Create a try build
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                // Check info
                tester.post_comment("@bors info").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "- **Approved by:** @{}\n- **Priority:** 10\n- **Try build branch:** automation/bors/try",
                        User::default_user().name
                    )
                );
                Ok(tester)
            })
            .await;
    }
}
