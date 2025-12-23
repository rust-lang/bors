use std::sync::Arc;

use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequestNumber;

pub(super) async fn command_ping(
    repo: Arc<RepositoryState>,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    let mut msg = "Pong 🏓!".to_string();
    if repo.is_paused() {
        msg.push_str(" (bors is paused)");
    }
    repo.client
        .post_comment(pr_number, Comment::new(msg))
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
