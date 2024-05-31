use std::sync::Arc;

use crate::bors::Comment;
use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::github::PullRequest;

pub(super) async fn command_ping<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr.number, Comment::new("Pong 🏓!".to_string()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::mocks::run_test;

    #[sqlx::test]
    async fn test_ping(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors ping").await;
            assert_eq!(tester.get_comment().await, "Pong 🏓!");
            Ok(tester)
        })
        .await;
    }
}
