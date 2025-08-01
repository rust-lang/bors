use std::sync::Arc;

use crate::bors::RepositoryState;
use crate::bors::comment::pong_message;
use crate::github::PullRequestNumber;

pub(super) async fn command_ping(
    repo: Arc<RepositoryState>,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client.post_comment(pr_number, pong_message()).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn ping_command(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors ping").await?;
            assert_eq!(tester.get_comment(()).await?, "Pong 🏓!");
            Ok(())
        })
        .await;
    }
}
