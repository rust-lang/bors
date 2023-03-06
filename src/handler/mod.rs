use crate::config::RepositoryConfig;
use crate::github::{GithubRepoName, GithubUser, PullRequest};
use crate::permissions::{PermissionResolver, PermissionType};
use axum::async_trait;

/// Provides functionality for working with a remote repository.
#[async_trait]
pub trait RepositoryClient {
    async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()>;
}

/// Service that handles BORS requests for a single repository.
pub struct BorsHandler<Client> {
    repository: GithubRepoName,
    config: RepositoryConfig,
    permission_resolver: Box<dyn PermissionResolver>,
    client: Client,
}

impl<Client: RepositoryClient> BorsHandler<Client> {
    pub fn new(
        repository: GithubRepoName,
        config: RepositoryConfig,
        permission_resolver: Box<dyn PermissionResolver>,
        client: Client,
    ) -> Self {
        Self {
            repository,
            config,
            permission_resolver,
            client,
        }
    }

    pub fn repository(&self) -> &GithubRepoName {
        &self.repository
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub async fn ping(&self, pr: &PullRequest) -> anyhow::Result<()> {
        log::debug!("Executing ping on {}", self.repository);
        self.client.post_comment(&pr, "Pong 🏓!").await?;
        Ok(())
    }

    pub async fn enqueue_try_build(
        &self,
        pr: &PullRequest,
        author: &GithubUser,
    ) -> anyhow::Result<()> {
        log::debug!("Executing try on {}/{}", self.repository, pr.number);

        if !self
            .permission_resolver
            .has_permission(&author.username, PermissionType::Try)
            .await
        {
            self.client
                .post_comment(
                    pr,
                    &format!(
                        "@{}: :key: Insufficient privileges: not in try users",
                        author.username
                    ),
                )
                .await?;
            return Ok(());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::config::RepositoryConfig;
    use crate::github::{GithubRepoName, GithubUser, PullRequest};
    use crate::handler::{BorsHandler, RepositoryClient};
    use crate::permissions::{PermissionResolver, PermissionType};
    use axum::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct NoPermissions;

    #[async_trait]
    impl PermissionResolver for NoPermissions {
        async fn has_permission(&self, _username: &str, _permission: PermissionType) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct MockRepository {
        comments: HashMap<u64, Vec<String>>,
    }

    struct MockClient {
        repository: Arc<Mutex<MockRepository>>,
    }

    #[async_trait]
    impl RepositoryClient for MockClient {
        async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
            self.repository
                .lock()
                .unwrap()
                .comments
                .entry(pr.number)
                .or_default()
                .push(text.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_ping() {
        let state = create_mock_state();
        let handler = create_handler(state.clone());
        handler.ping(&create_pr(1)).await.unwrap();
        check_comments(&state, 1, &["Pong 🏓!"]);
    }

    #[tokio::test]
    async fn test_try_no_permission() {
        let state = create_mock_state();
        let handler = create_handler(state.clone());
        handler
            .enqueue_try_build(&create_pr(1), &create_user("foo"))
            .await
            .unwrap();
        check_comments(
            &state,
            1,
            &["@foo: :key: Insufficient privileges: not in try users"],
        );
    }

    fn check_comments(state: &Arc<Mutex<MockRepository>>, pr_number: u64, comments: &[&str]) {
        assert_eq!(
            state.lock().unwrap().comments.get(&pr_number).unwrap(),
            &comments
                .into_iter()
                .map(|&s| String::from(s))
                .collect::<Vec<_>>()
        );
    }

    fn create_pr(number: u64) -> PullRequest {
        PullRequest {
            number,
            head_label: "".to_string(),
            head_ref: "".to_string(),
            base_ref: "".to_string(),
        }
    }

    fn create_user(username: &str) -> GithubUser {
        GithubUser {
            username: username.to_string(),
            html_url: format!("https://github.com/{username}").parse().unwrap(),
        }
    }

    fn create_mock_state() -> Arc<Mutex<MockRepository>> {
        Arc::new(Mutex::new(MockRepository::default()))
    }

    fn create_handler(repository: Arc<Mutex<MockRepository>>) -> BorsHandler<MockClient> {
        BorsHandler::new(
            GithubRepoName::new("foo", "bar"),
            RepositoryConfig { checks: vec![] },
            Box::new(NoPermissions),
            MockClient { repository },
        )
    }
}
