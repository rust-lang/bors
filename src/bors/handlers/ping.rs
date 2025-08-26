use std::sync::Arc;

use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequestNumber;

pub(super) async fn command_ping(
    repo: Arc<RepositoryState>,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr_number, Comment::new("Pong 🏓!".to_string()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn ping_command(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors ping").await?;
            assert_eq!(tester.get_comment_text(()).await?, "Pong 🏓!");
            Ok(())
        })
        .await;
    }
}
