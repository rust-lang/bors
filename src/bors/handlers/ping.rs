use crate::bors::RepositoryClient;
use crate::bors::RepositoryState;
use crate::github::PullRequest;

pub async fn command_ping<Client: RepositoryClient>(
    repo: &mut RepositoryState<Client>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    log::debug!("Executing ping on {}", repo.client.repository());
    repo.client
        .post_comment(pr.number.into(), "Pong 🏓!")
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::ping::command_ping;
    use crate::tests::client::RepoBuilder;
    use crate::tests::github::PRBuilder;

    #[tokio::test]
    async fn test_ping() {
        let mut repo = RepoBuilder::default().create();
        command_ping(&mut repo, &PRBuilder::default().number(1).create())
            .await
            .unwrap();
        repo.client.check_comments(1, &["Pong 🏓!"]);
    }
}
