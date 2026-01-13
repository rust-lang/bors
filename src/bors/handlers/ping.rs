use std::sync::Arc;

use crate::PgDbClient;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequestNumber;

pub(super) async fn command_ping(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr_number, Comment::new("Pong 🏓!".to_string()), db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn ping_command(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors ping").await?;
            assert_eq!(ctx.get_next_comment_text(()).await?, "Pong 🏓!");
            Ok(())
        })
        .await;
    }
}
