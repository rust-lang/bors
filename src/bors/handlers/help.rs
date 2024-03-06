use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::github::PullRequest;

const HELP_MESSAGE: &str = r#"
- try: Execute the `try` CI workflow on this PR (without approving it for a merge).
- try cancel: Stop try builds on current PR.
- ping: pong
- help: Print this help message
"#;

pub(super) async fn command_help<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    repo.client.post_comment(pr.number, HELP_MESSAGE).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::help::HELP_MESSAGE;
    use crate::tests::event::default_pr_number;
    use crate::tests::state::ClientBuilder;

    #[tokio::test]
    async fn test_help() {
        let mut state = ClientBuilder::default().create_state().await;
        state.comment("@bors help").await;
        state
            .client()
            .check_comments(default_pr_number(), &[HELP_MESSAGE]);
    }
}
