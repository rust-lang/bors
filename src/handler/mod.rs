use crate::config::RepositoryConfig;
use crate::github::GithubRepoName;
use crate::permissions::PermissionResolver;
use axum::async_trait;

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: u32,
}

/// Provides functionality for working with a remote repository.
#[async_trait]
pub trait RepositoryClient {
    async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()>;
}

/// Service that handles BORS requests for a single repository.
pub struct BorsHandler {
    repository: GithubRepoName,
    config: RepositoryConfig,
    permission_resolver: Box<dyn PermissionResolver>,
    client: Box<dyn RepositoryClient>,
}

impl BorsHandler {
    pub fn new(
        repository: GithubRepoName,
        config: RepositoryConfig,
        permission_resolver: Box<dyn PermissionResolver>,
        client: Box<dyn RepositoryClient>,
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

    pub fn client(&self) -> &dyn RepositoryClient {
        self.client.as_ref()
    }

    pub async fn ping(&self, pr: PullRequest) -> anyhow::Result<()> {
        log::debug!("Executing ping");
        self.client.post_comment(&pr, "Pong 🏓!").await?;
        Ok(())
    }

    async fn enqueue_try_build(&self, pr: &PullRequest) {}

    /*async fn try_merge(
            &self,
            repo: &RepositoryClient,
            author: &GithubUser,
            pr: PullRequest,
        ) -> anyhow::Result<()> {
            log::debug!("Executing try on {}/{}", repo.name(), pr.number);

            let branch_label = pr.head.label.unwrap_or_else(|| "<unknown>".to_string());
            let merge_msg = format!(
                "Auto merge of #{} - {}, r={}",
                pr.number, branch_label, author.username
            );

            let base = pr.base.ref_field;
            let head = pr.head.ref_field;

            // Just merge the PR directly right now. Let's hope that tests pass :)
            let result = merge_branches(repo, &base, &head, &merge_msg).await;
            log::debug!("Result of merge: {result:?}");

            let response = match result {
                Ok(_) => None,
                Err(error) => match error {
                    MergeError::NotFound => {
                        Some(format!("Base `{base}` or head `{head}` were not found."))
                    }
                    MergeError::Conflict => Some(merge_conflict_message(&head)),
                    MergeError::AlreadyMerged => Some("This branch was already merged.".to_string()),
                    result @ MergeError::Unknown { .. } | result @ MergeError::NetworkError(_) => {
                        log::error!(
                            "Failed to merge {branch_label} into head on {}: {:?}",
                            repo.name(),
                            result
                        );
                        Some("An error has occurred. Please try again later.".to_string())
                    }
                },
            };
            if let Some(message) = response {
                post_pr_comment(repo, pr.number, &message)
                    .await
                    .context("Cannot send PR comment")?;
            }
            Ok(())
        }

    fn merge_conflict_message(branch: &str) -> String {
        format!(
            r#"Merge conflict

    This pull request and the head branch diverged in a way that cannot
    be automatically merged. Please rebase on top of the latest master
    branch, and let the reviewer approve again.

    <details><summary>How do I rebase?</summary>

    Assuming `self` is your fork and `upstream` is this repository,
    you can resolve the conflict following these steps:

    1. `git checkout {branch}` *(switch to your branch)*
    2. `git fetch upstream master` *(retrieve the latest master)*
    3. `git rebase upstream/master -p` *(rebase on top of it)*
    4. Follow the on-screen instruction to resolve conflicts
     (check `git status` if you got lost).
    5. `git push self {branch} --force-with-lease` *(update this PR)*

    You may also read
    [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html) # noqa
    for a short tutorial.

    Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub. # noqa
    It uses `git merge` instead of `git rebase` which makes the PR commit # noqa
    history more difficult to read.

    Sometimes step 4 will complete without asking for resolution. This is
    usually due to difference between how `Cargo.lock` conflict is
    handled during merge and rebase. This is normal, and you should still # noqa
    perform step 5 to update this PR.

    </details>
    "#
        )
    }
        */
}

#[cfg(test)]
mod tests {
    use crate::config::RepositoryConfig;
    use crate::github::GithubRepoName;
    use crate::handler::{BorsHandler, PullRequest, RepositoryClient};
    use crate::permissions::{PermissionResolver, PermissionType};
    use axum::async_trait;
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    struct NoPermissions;

    #[async_trait]
    impl PermissionResolver for NoPermissions {
        async fn has_permission(&mut self, username: &str, permission: PermissionType) -> bool {
            false
        }
    }

    #[derive(Default)]
    struct MockRepository {
        comments: HashMap<u32, Vec<String>>,
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
        handler.ping(PullRequest { number: 1 }).await.unwrap();
        assert_eq!(
            state.lock().unwrap().comments.get(&1).unwrap(),
            &vec!["Pong 🏓!".to_string()]
        );
    }

    fn create_mock_state() -> Arc<Mutex<MockRepository>> {
        Arc::new(Mutex::new(MockRepository::default()))
    }

    fn create_handler(repository: Arc<Mutex<MockRepository>>) -> BorsHandler {
        BorsHandler::new(
            GithubRepoName::new("foo", "bar"),
            RepositoryConfig { checks: vec![] },
            Box::new(NoPermissions),
            Box::new(MockClient { repository }),
        )
    }
}
