use std::collections::HashMap;

use anyhow::Context;
use axum::async_trait;
use octocrab::models::{App, Repository};
use octocrab::{Error, Octocrab};
use tracing::log;

use crate::bors::event::PullRequestComment;
use crate::bors::{
    CheckSuite, CheckSuiteStatus, Comment, RepositoryClient, RepositoryLoader, RepositoryState,
};
use crate::config::{RepositoryConfig, CONFIG_FILE_PATH};
use crate::database::RunId;
use crate::github::api::base_github_html_url;
use crate::github::api::operations::{merge_branches, set_branch_to_commit, MergeError};
use crate::github::{Branch, CommitSha, GithubRepoName, PullRequest, PullRequestNumber};
use crate::permissions::TeamApiClient;

use super::load_repositories;

/// Provides access to a single app installation (repository) using the GitHub API.
pub struct GithubRepositoryClient {
    pub app: App,
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    pub client: Octocrab,
    // We store the name separately, because repository has an optional owner, but at this point
    // we must always have some owner of the repo.
    pub repo_name: GithubRepoName,
    pub repository: Repository,
}

impl GithubRepositoryClient {
    pub fn client(&self) -> &Octocrab {
        &self.client
    }

    pub fn name(&self) -> &GithubRepoName {
        &self.repo_name
    }

    fn format_pr(&self, pr: PullRequestNumber) -> String {
        format!("{}/{}/{}", self.name().owner(), self.name().name(), pr)
    }
}

impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    /// Was the comment created by the bot?
    async fn is_comment_internal(&self, comment: &PullRequestComment) -> anyhow::Result<bool> {
        Ok(comment.author.html_url == self.app.html_url)
    }

    /// Loads repository configuration from a file located at `[CONFIG_FILE_PATH]` in the main
    /// branch.
    async fn load_config(&self) -> anyhow::Result<RepositoryConfig> {
        let mut response = self
            .client
            .repos(&self.repo_name.owner, &self.repo_name.name)
            .get_content()
            .path(CONFIG_FILE_PATH)
            .send()
            .await
            .map_err(|error| {
                anyhow::anyhow!(
                    "Could not fetch {CONFIG_FILE_PATH} from {}: {error:?}",
                    self.repo_name
                )
            })?;

        response
            .take_items()
            .into_iter()
            .next()
            .and_then(|content| content.decoded_content())
            .ok_or_else(|| anyhow::anyhow!("Configuration file not found"))
            .and_then(|content| {
                let config: RepositoryConfig = toml::from_str(&content).map_err(|error| {
                    anyhow::anyhow!("Could not deserialize repository config: {error:?}")
                })?;
                Ok(config)
            })
    }

    async fn get_branch_sha(&self, name: &str) -> anyhow::Result<CommitSha> {
        // https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#get-a-branch
        let branch: octocrab::models::repos::Branch = self
            .client
            .get(
                format!(
                    "/repos/{}/{}/branches/{name}",
                    self.repo_name.owner(),
                    self.repo_name.name(),
                )
                .as_str(),
                None::<&()>,
            )
            .await
            .context("Cannot deserialize branch")?;
        Ok(CommitSha(branch.commit.sha))
    }

    async fn get_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        let pr = self
            .client
            .pulls(self.repository().owner(), self.repository().name())
            .get(pr.0)
            .await
            .map_err(|error| {
                anyhow::anyhow!("Could not get PR {}/{}: {error:?}", self.repository(), pr.0)
            })?;
        Ok(github_pr_to_pr(pr))
    }

    /// The comment will be posted as the Github App user of the bot.
    async fn post_comment(&self, pr: PullRequestNumber, comment: Comment) -> anyhow::Result<()> {
        self.client
            .issues(&self.name().owner, &self.name().name)
            .create_comment(pr.0, comment.render())
            .await
            .with_context(|| format!("Cannot post comment to {}", self.format_pr(pr)))?;
        Ok(())
    }

    async fn set_branch_to_sha(&self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        Ok(set_branch_to_commit(self, branch.to_string(), sha).await?)
    }

    async fn merge_branches(
        &self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        Ok(merge_branches(self, base, head, commit_message).await?)
    }

    async fn get_check_suites_for_commit(
        &self,
        branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>> {
        #[derive(serde::Deserialize, Debug)]
        struct CheckSuitePayload {
            conclusion: Option<String>,
            head_branch: String,
        }

        #[derive(serde::Deserialize, Debug)]
        struct CheckSuiteResponse {
            check_suites: Vec<CheckSuitePayload>,
        }

        let response: CheckSuiteResponse = self
            .client
            .get(
                format!(
                    "/repos/{}/{}/commits/{}/check-suites",
                    self.repo_name.owner(),
                    self.repo_name.name(),
                    sha.0
                )
                .as_str(),
                None::<&()>,
            )
            .await
            .context("Cannot fetch CheckSuiteResponse")?;

        let suites = response
            .check_suites
            .into_iter()
            .filter(|suite| suite.head_branch == branch)
            .map(|suite| CheckSuite {
                status: match suite.conclusion {
                    Some(status) => match status.as_str() {
                        "success" => CheckSuiteStatus::Success,
                        "failure" | "neutral" | "cancelled" | "skipped" | "timed_out"
                        | "action_required" | "startup_failure" | "stale" => {
                            CheckSuiteStatus::Failure
                        }
                        _ => {
                            tracing::warn!(
                                "Received unknown check suite status for {}/{}: {status}",
                                self.repo_name,
                                sha
                            );
                            CheckSuiteStatus::Pending
                        }
                    },
                    None => CheckSuiteStatus::Pending,
                },
            })
            .collect();
        Ok(suites)
    }

    async fn cancel_workflows(&self, run_ids: &[RunId]) -> anyhow::Result<()> {
        let actions = self.client.actions();

        // Cancel all workflows in parallel
        futures::future::join_all(run_ids.iter().map(|run_id| {
            actions.cancel_workflow_run(
                self.repo_name.owner(),
                self.repo_name.name(),
                (*run_id).into(),
            )
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        Ok(())
    }

    async fn add_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        let client = self.client.issues(self.name().owner(), self.name().name());
        if !labels.is_empty() {
            client
                .add_labels(pr.0, labels)
                .await
                .context("Cannot add label(s) to PR")?;
        }

        Ok(())
    }

    async fn remove_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        let client = self.client.issues(self.name().owner(), self.name().name());
        // The GitHub API only allows removing labels one by one, so we remove all of them in
        // parallel to speed it up a little.
        let labels_to_remove_futures = labels.iter().map(|label| client.remove_label(pr.0, label));
        futures::future::join_all(labels_to_remove_futures)
            .await
            .into_iter()
            .filter(|result| match result {
                Ok(_) => false,
                Err(error) => match error {
                    // This error is returned if we try to remove a label that does not exist on the issue.
                    // This should be a no-op, rather than an error, therefore we swallow this error.
                    Error::GitHub { source, .. }
                        if source.message.contains("Label does not exist") =>
                    {
                        log::trace!("Trying to remove label which does not exist on PR {pr}");
                        false
                    }
                    _ => true,
                },
            })
            .collect::<Result<Vec<_>, _>>()
            .context("Cannot remove label(s) from PR")?;

        Ok(())
    }

    fn get_workflow_url(&self, run_id: RunId) -> String {
        let html_url = self
            .repository
            .html_url
            .as_ref()
            .map(|url| url.to_string())
            .unwrap_or_else(|| {
                format!(
                    "{}/{}/{}",
                    base_github_html_url(),
                    self.name().owner,
                    self.name().name
                )
            });
        format!("{html_url}/actions/runs/{run_id}")
    }
}

#[async_trait]
impl RepositoryLoader<GithubRepositoryClient> for Octocrab {
    async fn load_repositories(
        &self,
        team_api_client: &TeamApiClient,
    ) -> anyhow::Result<
        HashMap<GithubRepoName, anyhow::Result<RepositoryState<GithubRepositoryClient>>>,
    > {
        load_repositories(self, team_api_client).await
    }
}

fn github_pr_to_pr(pr: octocrab::models::pulls::PullRequest) -> PullRequest {
    PullRequest {
        number: pr.number.into(),
        head_label: pr.head.label.unwrap_or_else(|| "<unknown>".to_string()),
        head: Branch {
            name: pr.head.ref_field,
            sha: pr.head.sha.into(),
        },
        base: Branch {
            name: pr.base.ref_field,
            sha: pr.base.sha.into(),
        },
        title: pr.title.unwrap_or_default(),
        message: pr.body.unwrap_or_default(),
    }
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::permissions::PermissionType;
    use crate::tests::mocks::Permissions;
    use crate::tests::mocks::{ExternalHttpMock, Repo};
    use crate::tests::mocks::{User, World};
    use crate::RepositoryLoader;
    use octocrab::models::UserId;

    #[tokio::test]
    async fn test_load_installed_repos() {
        let mock = ExternalHttpMock::start(
            &World::new()
                .add_repo(
                    Repo::new("foo", "bar", Permissions::default(), "".to_string())
                        .with_perms(User::new(1, "user"), &[PermissionType::Try]),
                )
                .add_repo(Repo::new(
                    "foo",
                    "baz",
                    Permissions::default(),
                    "".to_string(),
                )),
        )
        .await;
        let client = mock.github_client();
        let team_api_client = mock.team_api_client();
        let mut repos = client.load_repositories(&team_api_client).await.unwrap();
        assert_eq!(repos.len(), 2);

        let repo = repos
            .remove(&GithubRepoName::new("foo", "bar"))
            .unwrap()
            .unwrap();
        assert!(repo
            .permissions
            .load()
            .has_permission(UserId(1), PermissionType::Try));
        assert!(!repo
            .permissions
            .load()
            .has_permission(UserId(1), PermissionType::Review));
    }
}
