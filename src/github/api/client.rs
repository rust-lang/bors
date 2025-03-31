use anyhow::Context;
use octocrab::models::{App, Repository};
use octocrab::{Error, Octocrab};
use tracing::log;

use crate::bors::event::PullRequestComment;
use crate::bors::{CheckSuite, CheckSuiteStatus, Comment};
use crate::config::{CONFIG_FILE_PATH, RepositoryConfig};
use crate::database::RunId;
use crate::github::api::base_github_html_url;
use crate::github::api::operations::{MergeError, merge_branches, set_branch_to_commit};
use crate::github::{CommitSha, GithubRepoName, PullRequest, PullRequestNumber};
use crate::utils::timing::measure_network_request;

/// Provides access to a single app installation (repository) using the GitHub API.
pub struct GithubRepositoryClient {
    app: App,
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    client: Octocrab,
    // We store the name separately, because repository has an optional owner, but at this point
    // we must always have some owner of the repo.
    repo_name: GithubRepoName,
    repository: Repository,
}

impl GithubRepositoryClient {
    pub fn new(
        app: App,
        client: Octocrab,
        repo_name: GithubRepoName,
        repository: Repository,
    ) -> Self {
        Self {
            app,
            client,
            repo_name,
            repository,
        }
    }

    pub fn client(&self) -> &Octocrab {
        &self.client
    }

    pub fn repository(&self) -> &GithubRepoName {
        &self.repo_name
    }

    /// Was the comment created by the bot?
    pub async fn is_comment_internal(&self, comment: &PullRequestComment) -> anyhow::Result<bool> {
        Ok(comment.author.html_url == self.app.html_url)
    }

    /// Loads repository configuration from a file located at `[CONFIG_FILE_PATH]` in the main
    /// branch.
    pub async fn load_config(&self) -> anyhow::Result<RepositoryConfig> {
        measure_network_request("load_config", || async {
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
        })
        .await
    }

    /// Return the current SHA of the given branch.
    pub async fn get_branch_sha(&self, name: &str) -> anyhow::Result<CommitSha> {
        measure_network_request("get_branch_sha", || async {
            // https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#get-a-branch
            let branch: octocrab::models::repos::Branch = self
                .client
                .get(
                    format!("/repos/{}/branches/{name}", self.repository()).as_str(),
                    None::<&()>,
                )
                .await
                .context("Cannot deserialize branch")?;
            Ok(CommitSha(branch.commit.sha))
        })
        .await
    }

    /// Resolve a pull request from this repository by it's number.
    pub async fn get_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        measure_network_request("get_pull_request", || async {
            let pr = self
                .client
                .pulls(self.repository().owner(), self.repository().name())
                .get(pr.0)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Could not get PR {}/{}: {error:?}", self.repository(), pr.0)
                })?;
            Ok(pr.into())
        })
        .await
    }

    /// Submit a pull request approval review.
    pub async fn approve_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<()> {
        measure_network_request("approve_pull_request", || async {
            #[derive(serde::Serialize)]
            struct ReviewRequest {
                event: &'static str,
            }

            let request = ReviewRequest { event: "APPROVE" };

            self.client
                ._post(
                    format!(
                        "/repos/{}/{}/pulls/{}/reviews",
                        self.repo_name.owner(),
                        self.repo_name.name(),
                        pr.0
                    )
                    .as_str(),
                    Some(&request),
                )
                .await?;

            Ok(())
        })
        .await
    }

    /// Submit a review that removes approval.
    pub async fn unapprove_pull_request(
        &self,
        pr: PullRequestNumber,
        message: String,
    ) -> anyhow::Result<()> {
        measure_network_request("submit_unapproval", || async {
            #[derive(serde::Serialize)]
            struct ReviewRequest {
                event: &'static str,
                body: String,
            }

            let request = ReviewRequest {
                event: "COMMENT",
                body: message,
            };

            self.client
                ._post(
                    format!(
                        "/repos/{}/{}/pulls/{}/reviews",
                        self.repo_name.owner(),
                        self.repo_name.name(),
                        pr.0
                    )
                    .as_str(),
                    Some(&request),
                )
                .await?;

            Ok(())
        })
        .await
    }

    /// Post a comment to the pull request with the given number.
    /// The comment will be posted as the Github App user of the bot.
    pub async fn post_comment(
        &self,
        pr: PullRequestNumber,
        comment: Comment,
    ) -> anyhow::Result<()> {
        measure_network_request("post_comment", || async {
            self.client
                .issues(&self.repository().owner, &self.repository().name)
                .create_comment(pr.0, comment.render())
                .await
                .with_context(|| format!("Cannot post comment to {}", self.format_pr(pr)))?;
            Ok(())
        })
        .await
    }

    /// Set the given branch to a commit with the given `sha`.
    pub async fn set_branch_to_sha(&self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        measure_network_request("set_branch_to_sha", || async {
            Ok(set_branch_to_commit(self, branch.to_string(), sha).await?)
        })
        .await
    }

    /// Merge `head` into `base`. Returns the SHA of the merge commit.
    pub async fn merge_branches(
        &self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        measure_network_request("merge_branches", || async {
            merge_branches(self, base, head, commit_message).await
        })
        .await
    }

    /// Find all check suites attached to the given commit and branch.
    pub async fn get_check_suites_for_commit(
        &self,
        branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>> {
        measure_network_request("get_check_suites_for_commit", || async {
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
        })
        .await
    }

    /// Cancels Github Actions workflows.
    pub async fn cancel_workflows(&self, run_ids: &[RunId]) -> anyhow::Result<()> {
        measure_network_request("cancel_workflows", || async {
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
        })
        .await
    }

    /// Add a set of labels to a PR.
    pub async fn add_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        measure_network_request("add_labels", || async {
            let client = self
                .client
                .issues(self.repository().owner(), self.repository().name());
            if !labels.is_empty() {
                client
                    .add_labels(pr.0, labels)
                    .await
                    .context("Cannot add label(s) to PR")?;
            }

            Ok(())
        })
        .await
    }

    /// Remove a set of labels from a PR.
    pub async fn remove_labels(
        &self,
        pr: PullRequestNumber,
        labels: &[String],
    ) -> anyhow::Result<()> {
        measure_network_request("remove_labels", || async {
            let client = self
                .client
                .issues(self.repository().owner(), self.repository().name());
            // The GitHub API only allows removing labels one by one, so we remove all of them in
            // parallel to speed it up a little.
            let labels_to_remove_futures =
                labels.iter().map(|label| client.remove_label(pr.0, label));
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
        })
        .await
    }

    /// Get a workflow url.
    pub fn get_workflow_url(&self, run_id: RunId) -> String {
        let html_url = self
            .repository
            .html_url
            .as_ref()
            .map(|url| url.to_string())
            .unwrap_or_else(|| format!("{}/{}", base_github_html_url(), self.repository(),));
        format!("{html_url}/actions/runs/{run_id}")
    }

    /// Get workflow url for a list of workflows.
    pub fn get_workflow_urls<'a>(
        &'a self,
        run_ids: impl Iterator<Item = RunId> + 'a,
    ) -> impl Iterator<Item = String> + 'a {
        run_ids.map(|workflow_id| self.get_workflow_url(workflow_id))
    }

    fn format_pr(&self, pr: PullRequestNumber) -> String {
        format!("{}/{}", self.repository(), pr)
    }
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::github::api::load_repositories;
    use crate::permissions::PermissionType;
    use crate::tests::mocks::Permissions;
    use crate::tests::mocks::{ExternalHttpMock, Repo};
    use crate::tests::mocks::{GitHubState, User};
    use octocrab::models::UserId;

    #[tokio::test]
    async fn load_installed_repos() {
        let mock = ExternalHttpMock::start(
            &GitHubState::new()
                .with_repo(
                    Repo::new(
                        GithubRepoName::new("foo", "bar"),
                        Permissions::empty(),
                        "".to_string(),
                    )
                    .with_user_perms(User::new(1, "user"), &[PermissionType::Try]),
                )
                .with_repo(Repo::new(
                    GithubRepoName::new("foo", "baz"),
                    Permissions::empty(),
                    "".to_string(),
                )),
        )
        .await;
        let client = mock.github_client();
        let team_api_client = mock.team_api_client();
        let mut repos = load_repositories(&client, &team_api_client).await.unwrap();
        assert_eq!(repos.len(), 2);

        let repo = repos
            .remove(&GithubRepoName::new("foo", "bar"))
            .unwrap()
            .unwrap();
        assert!(
            repo.permissions
                .load()
                .has_permission(UserId(1), PermissionType::Try)
        );
        assert!(
            !repo
                .permissions
                .load()
                .has_permission(UserId(1), PermissionType::Review)
        );
    }
}
