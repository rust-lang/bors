use std::sync::Arc;

use crate::bors::comments::HelpComment;
use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::github::PullRequest;

pub(super) async fn command_help<Client: RepositoryClient>(
    repo: Arc<RepositoryState<Client>>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr.number, Box::new(HelpComment))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::comments::HelpComment;
    use crate::bors::Comment;
    use crate::tests::event::default_pr_number;
    use crate::tests::state::ClientBuilder;

    #[tokio::test]
    async fn test_help() {
        let state = ClientBuilder::default().create_state().await;
        state.comment("@bors help").await;
        state
            .client()
            .check_comments(default_pr_number(), &[HelpComment.render().as_str()]);
    }
}
