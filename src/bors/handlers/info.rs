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
    let pr_model = db.get_pull_request(repo.repository(), pr.number).await?;

    // Building the info message
    let mut info_lines = Vec::new();

    // Approval info
    if let Some(pr) = pr_model {
        if let Some(approved_by) = pr.approved_by {
            info_lines.push(format!("- **Approved by:** @{}", approved_by));
        } else {
            info_lines.push("- **Approved by:** No one".to_string());
        }

        // Priority info
        if let Some(priority) = pr.priority {
            info_lines.push(format!("- **Priority:** {}", priority));
        } else {
            info_lines.push("- **Priority:** None".to_string());
        }

        // Build status
        if let Some(try_build) = pr.try_build {
            info_lines.push(format!("- **Try build branch:** {}", try_build.branch));
        } else {
            info_lines.push("- **Try build:** None".to_string());
        }
    } else {
        info_lines.push("- **PR not found in database**".to_string());
    }

    // Merge status (placeholder for future)
    info_lines.push("- **Merge status:** Not implemented yet".to_string());

    // Queue link (placeholder for future)
    info_lines.push("- **Queue link:** Not implemented yet".to_string());

    // Joining all lines
    let info = info_lines.join("\n");

    // Post the comment
    repo.client
        .post_comment(pr.number, Comment::new(info))
        .await?;

    Ok(())
}
