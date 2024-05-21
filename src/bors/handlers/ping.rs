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
    use crate::tests::event::default_pr_number;
    use crate::tests::state::ClientBuilder;

    #[sqlx::test]
    async fn test_ping(pool: sqlx::PgPool) {
        let state = ClientBuilder::default().pool(pool).create_state().await;
        state.comment("@bors ping").await;
        state
            .client()
            .check_comments(default_pr_number(), &["Pong 🏓!"]);
    }
}
