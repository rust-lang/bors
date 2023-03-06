use crate::github::PullRequest;
use crate::handlers::RepositoryClient;

pub async fn command_ping<Client: RepositoryClient>(
    client: &Client,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    log::debug!("Executing ping on {}", client.repository());
    client.post_comment(pr, "Pong 🏓!").await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::handlers::ping::command_ping;
    use crate::tests::client::test_client;
    use crate::tests::model::create_pr;

    #[tokio::test]
    async fn test_ping() {
        let client = test_client();
        command_ping(&client, &create_pr(1)).await.unwrap();
        client.check_comments(1, &["Pong 🏓!"]);
    }
}
