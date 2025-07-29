use octocrab::Octocrab;
use std::collections::HashMap;
use wiremock::MockServer;

use crate::create_github_client;
use crate::github::GithubRepoName;
use crate::tests::mocks::GitHubState;
use crate::tests::mocks::app::{AppHandler, default_app_id};
use crate::tests::mocks::comment::Comment;
use crate::tests::mocks::repository::{mock_repo, mock_repo_list};

/// Represents the state of a simulated GH repo.
pub struct GitHubRepoState {
    // We store comments from all PRs inside a single queue, because
    // we don't necessarily know beforehand which PRs will receive comments.
    comments_queue: tokio::sync::mpsc::Receiver<Comment>,
    // We need a local queue to avoid skipping comments received out of order.
    pending_comments: Vec<Comment>,
}

impl GitHubRepoState {
    /// Wait until a comment from the given pull request was received.
    /// If a comment from a different PR is received, it is inserted into
    /// `pending_comments`, to be picked up later.
    async fn get_comment(&mut self, pr: u64) -> Comment {
        // First, try to resolve the comment from the pending comment list
        if let Some(index) = self.pending_comments.iter().position(|c| c.pr == pr) {
            return self.pending_comments.remove(index);
        }
        // If it is not there, wait until some comment is received
        loop {
            let comment = self
                .comments_queue
                .recv()
                .await
                .expect("Channel was closed while waiting for a comment");

            if comment.pr == pr {
                return comment;
            }
            tracing::warn!(
                "Received comment for PR {}, while expected for PR {pr}",
                comment.pr
            );
            self.pending_comments.push(comment);
        }
    }
}

pub struct GitHubMockServer {
    mock_server: MockServer,
    repos: HashMap<GithubRepoName, GitHubRepoState>,
}

impl GitHubMockServer {
    pub async fn start(github: &GitHubState) -> Self {
        let mock_server = MockServer::start().await;
        mock_repo_list(github, &mock_server).await;

        // Repositories are mocked separately to make it easier to
        // pass comm. channels to them.
        let mut repos = HashMap::default();
        for (name, repo) in &github.repos {
            let (comments_tx, comments_rx) = tokio::sync::mpsc::channel(1024);
            mock_repo(repo.clone(), comments_tx, &mock_server).await;
            repos.insert(
                name.clone(),
                GitHubRepoState {
                    comments_queue: comments_rx,
                    pending_comments: Default::default(),
                },
            );
        }

        AppHandler::default().mount(&mock_server).await;

        Self { mock_server, repos }
    }

    pub fn client(&self) -> Octocrab {
        create_github_client(
            default_app_id().into(),
            self.mock_server.uri(),
            GITHUB_MOCK_PRIVATE_KEY.into(),
        )
        .unwrap()
    }

    pub async fn get_comment(
        &mut self,
        repo_name: GithubRepoName,
        pr: u64,
    ) -> anyhow::Result<Comment> {
        let repo = self
            .repos
            .get_mut(&repo_name)
            .unwrap_or_else(|| panic!("Repository `{repo_name}` not found"));
        let comment = repo.get_comment(pr).await;
        eprintln!("Received comment on {repo_name}#{pr}: {}", comment.content);
        Ok(comment)
    }

    /// Make sure that there are no leftover events left in the queues.
    pub async fn assert_empty_queues(mut self) {
        // This will remove all mocks and thus also any leftover
        // channel senders, so that we can be sure below that the `recv`
        // call will not block indefinitely.
        self.mock_server.reset().await;
        drop(self.mock_server);

        for (name, repo) in self.repos.iter_mut() {
            if !repo.pending_comments.is_empty() {
                panic!(
                    "Expected that {name} won't have any received comments, but it has {:?}",
                    repo.pending_comments
                );
            }
            // Make sure that the queue has been closed and nothing is in it.
            if let Some(comment) = repo.comments_queue.recv().await {
                panic!(
                    "Expected that {name} won't have any received comments, but it has {comment:?}"
                );
            }
        }
    }
}

const GITHUB_MOCK_PRIVATE_KEY: &str = r###"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDa2WIpKFzkeys9
R1Mn+kCM+UVAFT47QK98iTcXIG982rXPYdL2+jtzUoUc7irfZnScIiFmO8U2O2Nm
nchI/bjdceGGxyCt04PdYVgYpUWfqp2rbiqi6V6L39j23zxGUbf8chQsviW7HuCl
B105H/DQdhuHDmhOOWBg1Hv+uepp0pF86WPiz4/Ez9uNmIsoJps2pvOIi8nf/8k2
iRClRynYbE8495rD5ZGRHRkPG4wXYoKv/iH8Q7+kO+72sBk0oqmkEoUdUz+itQOB
CnWZWBC5vXb+vopw30vRZi0FNoArzN5WH2pXUKuFYkIIBWb9Aq6TDmsjPPth/VCb
DBaSJuM7AgMBAAECggEAOFkERyiXUlTMO0jkBkUO3b1IsUlG7qanCF+kCZZWXkVJ
zo2XbfPb3sN+doZ0D3UnzROUmegFzQLZgxBZA0IgmRO7R6J5rYfqSdPIhP/4vzWE
xyDkZXHE4CrQiC/OKyTbRGpy+1oyCM3YdWVCAXVR4bqnN8zj2lA3mnbbPijMTFZr
B67VdKF16qp9L2A8cEKdEHs+m86l/y6ySSpHHOLFCZblUDjeb2dqTg7DWod1Ughj
kj8FbyeNQovn0KMH3qkdYOAxC+paJaKYdd680ewKO1i+zbI6bauoUUZsLzKwdh5M
FrQhVpZ9Gc0Rroyyp85WMCCy1eyyHo732RuN0qyicQKBgQDg9+5p4XVVlcObjnHi
adfviAYxNbXu1T3S8Yejvva8c75ImHeo9pt0X4yUsnLuqflmM8UbzBbn8+PbXfux
rXiZPxl7wR7kDkKpmTwsTFOxYB2CgC4qetlcylG1kGSgckbz2dYYMmmK6ZauMBlU
dHfzaGJ8aUC2R/ZLl0ZxQVNNNQKBgQD5CV3vybh+RUCfm6qm6lGAs1ONFjjliwkQ
CE/bTGspBq37WlhWaeO+I3BkqivozlQ92jcGwxQq8oDhvEieucqDyYjUvp1Pxt1e
LT3gLy1kq1pREBI1bB1zCK1HpGn8eigIcuB8nOE+4tMUeKQNH++mhta7MgcSOoH9
4hIQgzAsrwKBgDnS4D/syGjoJq/8C/+jLvKNZvINGSc7PjnTBQcslWTY5ybnsZIH
WOuvh4XM3EfF/qmrUtWTPqv9/yoqXQBNUzsogddSSytZEv9euJ22PKjRyKP7aGJY
0zfLdPcTFxo6ZUxWSHZNtt0Srz00db5EdXRl9zJ9JznzAzZoup1vqgalAoGANRmw
M+7ZLeNqUh4JFyojUsPp7s1sOFWbCxYaoPH8b3UDJ/Mtns9ZRjOcRXqbfjpwb/fV
f9WcuUOYA4n4GhAXhF42lNZICLiofuo6pVCp5ys6SMqad1WkOeEBwaLnDnSlkJee
EjQJOzV2OIk4waurl+BsbOHP7C0Zhp7rpyWx4fUCgYEAp/4UceUfbJZGa8CcWZ8F
7M0LU9Q+tGPkP2W87zkzP82PF0bQCPT3dBP0ZduDchacrF0bqEcvMLWKwwUSNvkx
3VafpHjFw+fMUjcIkQk0VfdbRD5fLDQpJy6hUVq6A+duSqTvlhE8DFAdBAC3VZ9k
34PVnCZP7HB3k2eBSpDp4vk=
-----END PRIVATE KEY-----"###;
