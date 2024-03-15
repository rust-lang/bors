use anyhow::Context;
use axum::async_trait;
use octocrab::models::{Repository, RunId};
use octocrab::{Error, Octocrab};
use tracing::log;

use crate::bors::{CheckSuite, CheckSuiteStatus, RepositoryClient};
use crate::github::api::base_github_url;
use crate::github::api::operations::{merge_branches, set_branch_to_commit, MergeError};
use crate::github::{Branch, CommitSha, GithubRepoName, PullRequest, PullRequestNumber};

/// Provides access to a single app installation (repository) using the GitHub API.
pub struct GithubRepositoryClient {
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

#[async_trait]
impl RepositoryClient for GithubRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        self.name()
    }

    async fn get_branch_sha(&mut self, name: &str) -> anyhow::Result<CommitSha> {
        // https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#get-a-branch
        let branch: octocrab::models::repos::Branch = self
            .client
            .get(
                base_github_url()
                    .join(&format!(
                        "/repos/{}/{}/branches/{name}",
                        self.repo_name.owner(),
                        self.repo_name.name(),
                    ))?
                    .as_str(),
                None::<&()>,
            )
            .await
            .context("Cannot deserialize branch")?;
        Ok(CommitSha(branch.commit.sha))
    }

    async fn get_pull_request(&mut self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
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
    async fn post_comment(&mut self, pr: PullRequestNumber, text: &str) -> anyhow::Result<()> {
        self.client
            .issues(&self.name().owner, &self.name().name)
            .create_comment(pr.0, text)
            .await
            .with_context(|| format!("Cannot post comment to {}", self.format_pr(pr)))?;
        Ok(())
    }

    async fn set_branch_to_sha(&mut self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        Ok(set_branch_to_commit(self, branch.to_string(), sha).await?)
    }

    async fn merge_branches(
        &mut self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        Ok(merge_branches(self, base, head, commit_message).await?)
    }

    async fn get_check_suites_for_commit(
        &mut self,
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
                base_github_url()
                    .join(&format!(
                        "/repos/{}/{}/commits/{}/check-suites",
                        self.repo_name.owner(),
                        self.repo_name.name(),
                        sha.0
                    ))?
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

    async fn cancel_workflows(&mut self, run_ids: Vec<RunId>) -> anyhow::Result<()> {
        let actions = self.client.actions();

        // Cancel all workflows in parallel
        futures::future::join_all(run_ids.into_iter().map(|run_id| {
            actions.cancel_workflow_run(self.repo_name.owner(), self.repo_name.name(), run_id)
        }))
        .await
        .into_iter()
        .collect::<Result<Vec<_>, _>>()?;

        Ok(())
    }

    async fn add_labels(&mut self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        let client = self.client.issues(self.name().owner(), self.name().name());
        if !labels.is_empty() {
            client
                .add_labels(pr.0, labels)
                .await
                .context("Cannot add label(s) to PR")?;
        }

        Ok(())
    }

    async fn remove_labels(
        &mut self,
        pr: PullRequestNumber,
        labels: &[String],
    ) -> anyhow::Result<()> {
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
