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
    use crate::tests::tester::BorsTester;

    #[tokio::test]
    async fn test_ping() {
        let mut tester = BorsTester::new().await;
        tester.comment("@bors ping").await;
        tester.check_comments(&["Pong 🏓!"]).await;
    }
}
