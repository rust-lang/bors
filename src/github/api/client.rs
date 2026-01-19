use anyhow::Context;
use octocrab::Octocrab;
use octocrab::models::checks::CheckRun;
use octocrab::models::{CheckRunId, Repository, RunId};
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
use std::time::Duration;
use tracing::log;

use crate::PgDbClient;
use crate::bors::event::PullRequestComment;
use crate::bors::{Comment, WorkflowRun};
use crate::config::{CONFIG_FILE_PATH, RepositoryConfig, deserialize_config};
use crate::database::WorkflowStatus;
use crate::github::api::CommitAuthor;
use crate::github::api::operations::{
    BranchUpdateError, Commit, CommitCreateError, ForcePush, MergeError, create_branch,
    create_check_run, create_commit, merge_branches, set_branch_to_commit, update_check_run,
};
use crate::github::{CommitSha, GithubRepoName, PullRequest, PullRequestNumber, TreeSha};
use crate::utils::timing::{RetryMethod, RetryableOpError, ShouldRetry, perform_retryable};
use futures::TryStreamExt;
use octocrab::models::workflows::{Job, Run};
use serde::de::DeserializeOwned;
use url::Url;

/// Provides access to a single app installation (repository) using the GitHub API.
pub struct GithubRepositoryClient {
    /// HTML URL of the author, to recognize
    author_html_url: Url,
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    client: Octocrab,
    // We store the name separately, because repository has an optional owner, but at this point
    // we must always have some owner of the repo.
    repo_name: GithubRepoName,
}

impl GithubRepositoryClient {
    pub fn new(author_html_url: Url, client: Octocrab, repo_name: GithubRepoName) -> Self {
        Self {
            author_html_url,
            client,
            repo_name,
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
        Ok(comment.author.html_url == self.author_html_url)
    }

    /// Load repository information for the current client
    pub async fn get_repo(&self) -> anyhow::Result<Repository> {
        let repo = perform_retryable::<Repository, anyhow::Error, _, _, _>(
            "get_repo",
            RetryMethod::default(),
            || async {
                let response = self
                    .client
                    .repos(&self.repo_name.owner, &self.repo_name.name)
                    .get()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("Could not fetch repository {}: {error:?}", self.repo_name)
                    })?;
                anyhow::Ok(response)
            },
        )
        .await?;
        Ok(repo)
    }

    /// Create a new pull request at the specified repository.
    pub async fn create_pr(
        &self,
        repo: &GithubRepoName,
        title: &str,
        head: &str,
        base: &str,
        description: &str,
    ) -> anyhow::Result<PullRequest> {
        let pr = perform_retryable::<PullRequest, anyhow::Error, _, _, _>(
            "create_pr",
            RetryMethod::default(),
            || async {
                let pr = self
                    .client
                    .pulls(repo.owner(), repo.name())
                    .create(title, head, base)
                    .body(description)
                    .send()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Could not create pull request `{title}` in `{repo}`: {error:?}",
                        )
                    })?;
                anyhow::Ok(pr.into())
            },
        )
        .await?;
        Ok(pr)
    }

    /// Loads repository configuration from a file located at `[CONFIG_FILE_PATH]` in the main
    /// branch.
    pub async fn load_config(&self) -> anyhow::Result<RepositoryConfig> {
        let config = perform_retryable::<RepositoryConfig, anyhow::Error, _, _, _>(
            "load_config",
            RetryMethod::default(),
            || async {
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
                        let config: RepositoryConfig =
                            deserialize_config(&content).map_err(|error| {
                                anyhow::anyhow!(
                                    "Could not deserialize repository config: {error:?}"
                                )
                            })?;
                        Ok(config)
                    })
                    // If the config can't be found or parsed, there is no need for retrying at this
                    // moment
                    .map_err(ShouldRetry::No)
            },
        )
        .await?;
        Ok(config)
    }

    /// Return the current SHA of the given branch.
    pub async fn get_branch_sha(&self, name: &str) -> anyhow::Result<CommitSha> {
        let commit_sha = perform_retryable("get_branch_sha", RetryMethod::default(), || async {
            // https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#get-a-branch
            let branch: octocrab::models::repos::Branch = self
                .get_request(&format!("branches/{name}"))
                .await
                .context("Cannot deserialize branch")?;
            anyhow::Ok(CommitSha(branch.commit.sha))
        })
        .await?;
        Ok(commit_sha)
    }

    /// Resolve a pull request from this repository by it's number.
    pub async fn get_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        let prs = perform_retryable("get_pull_request", RetryMethod::default(), || async {
            let pr = self
                .client
                .pulls(self.repository().owner(), self.repository().name())
                .get(pr.0)
                .await
                .map_err(|error| {
                    anyhow::anyhow!("Could not get PR {}/{}: {error:?}", self.repository(), pr.0)
                })?;
            anyhow::Ok(PullRequest::from(pr))
        })
        .await?;
        Ok(prs)
    }

    pub async fn get_pull_request_commits(
        &self,
        number: PullRequestNumber,
    ) -> anyhow::Result<Vec<Commit>> {
        let commits = perform_retryable(
            "get_pull_request_commits",
            RetryMethod::default(),
            || async {
                let response = self
                    .client
                    .pulls(self.repository().owner(), self.repository().name())
                    .pr_commits(number.0)
                    .per_page(100)
                    .send()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "Could not get PR {}#{number} commits: {error:?}",
                            self.repository()
                        )
                    })?;

                let mut commits = Vec::new();
                let mut stream = std::pin::pin!(response.into_stream(&self.client));
                while let Some(gh_commit) = stream.try_next().await? {
                    let commit = Commit {
                        sha: CommitSha(gh_commit.sha),
                        parents: gh_commit
                            .parents
                            .into_iter()
                            .filter_map(|p| p.sha.map(CommitSha))
                            .collect(),
                        tree: TreeSha(gh_commit.commit.tree.sha),
                        author: CommitAuthor::from_gh(gh_commit.commit.author),
                        message: gh_commit.commit.message,
                    };
                    commits.push(commit);
                }
                anyhow::Ok(commits)
            },
        )
        .await?;
        Ok(commits)
    }

    /// Post a comment to the pull request with the given number.
    /// The comment will be posted as the Github App user of the bot.
    pub async fn post_comment(
        &self,
        pr: PullRequestNumber,
        comment: Comment,
        db: &PgDbClient,
    ) -> anyhow::Result<octocrab::models::issues::Comment> {
        let gh_comment = perform_retryable("post_comment", RetryMethod::default(), || async {
            self.client
                .issues(&self.repository().owner, &self.repository().name)
                .create_comment(pr.0, comment.render())
                .await
                .with_context(|| format!("Cannot post comment to {}", self.format_pr(pr)))
        })
        .await?;

        if let Some(tag) = comment.tag() {
            db.record_tagged_bot_comment(self.repository(), pr, tag, &gh_comment.node_id)
                .await?;
        }

        Ok(gh_comment)
    }

    /// Set the given branch to a commit with the given `sha`.
    pub async fn set_branch_to_sha(
        &self,
        branch: &str,
        sha: &CommitSha,
        force: ForcePush,
    ) -> Result<(), crate::github::api::operations::BranchUpdateError> {
        perform_retryable("set_branch_to_sha", RetryMethod::default(), || async {
            set_branch_to_commit(self, branch.to_string(), sha, force)
                .await
                .map_err(|e| match e {
                    error @ (BranchUpdateError::Conflict(_)
                    | BranchUpdateError::ValidationFailed(_)) => ShouldRetry::No(error),
                    error => ShouldRetry::Yes(error),
                })
        })
        .await
        .map_err(|error| match error {
            RetryableOpError::Err(error) => error,
            RetryableOpError::AllAttemptsExhausted(_) => BranchUpdateError::Timeout,
        })
    }

    /// Creates a new branch and pushes the given `sha` to it.
    pub async fn create_branch(&self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        perform_retryable("create_branch", RetryMethod::default(), || async {
            anyhow::Ok(
                create_branch(self, branch.to_string(), sha)
                    .await
                    .map_err(|err| anyhow::anyhow!("{err}"))?,
            )
        })
        .await?;
        Ok(())
    }

    /// Merge `head` into `base`. Returns the SHA of the merge commit.
    pub async fn merge_branches(
        &self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<Commit, MergeError> {
        perform_retryable(
            "merge_branches",
            RetryMethod::default().with_timeout(Duration::from_secs(30)),
            || async {
                merge_branches(self, base, head, commit_message)
                    .await
                    .map_err(|e| match e {
                        error @ (MergeError::AlreadyMerged
                        | MergeError::Conflict
                        | MergeError::NotFound) => ShouldRetry::No(error),
                        error => ShouldRetry::Yes(error),
                    })
            },
        )
        .await
        .map_err(|error| match error {
            RetryableOpError::Err(error) => error,
            RetryableOpError::AllAttemptsExhausted(_) => MergeError::Timeout,
        })
    }

    /// Create a new commit with the given author.
    pub async fn create_commit(
        &self,
        tree: &TreeSha,
        parents: &[CommitSha],
        message: &str,
        author: &CommitAuthor,
    ) -> Result<CommitSha, CommitCreateError> {
        perform_retryable("create_commit", RetryMethod::default(), || async {
            create_commit(self, tree, parents, message, author)
                .await
                .map_err(|e| match e {
                    error @ (CommitCreateError::ValidationFailed
                    | CommitCreateError::Conflict
                    | CommitCreateError::TreeOrParentsNotFound) => ShouldRetry::No(error),
                    error => ShouldRetry::Yes(error),
                })
        })
        .await
        .map_err(|error| match error {
            RetryableOpError::Err(error) => error,
            RetryableOpError::AllAttemptsExhausted(_) => CommitCreateError::Timeout,
        })
    }

    /// Create a check run for the given commit.
    pub async fn create_check_run(
        &self,
        name: &str,
        head_sha: &CommitSha,
        status: CheckRunStatus,
        output: CheckRunOutput,
        external_id: &str,
    ) -> anyhow::Result<CheckRun> {
        let check_run = perform_retryable("create_check_run", RetryMethod::no_retry(), || {
            let output = output.clone();
            async {
                create_check_run(self, name, head_sha, status, output, external_id)
                    .await
                    .context("Cannot create check run")
            }
        })
        .await?;
        Ok(check_run)
    }

    /// Update a check run with the given check run ID.
    pub async fn update_check_run(
        &self,
        check_run_id: CheckRunId,
        status: CheckRunStatus,
        conclusion: Option<CheckRunConclusion>,
    ) -> anyhow::Result<CheckRun> {
        let check_run = perform_retryable("update_check_run", RetryMethod::no_retry(), || async {
            update_check_run(self, check_run_id, status, conclusion)
                .await
                .context("Cannot update check run")
        })
        .await?;
        Ok(check_run)
    }

    /// Find all workflows attached to a specific commit SHA.
    pub async fn get_workflow_runs_for_commit_sha(
        &self,
        commit_sha: CommitSha,
    ) -> anyhow::Result<Vec<WorkflowRun>> {
        let runs = perform_retryable("get_workflows_for_commit_sha", RetryMethod::default(), || async {
            let response = self.client.workflows(self.repo_name.owner(), self.repo_name.name())
                .list_all_runs()
                .head_sha(&commit_sha.0)
                .send()
                .await?;
            let mut runs = Vec::with_capacity(
                response
                    .total_count
                    .map(|v| v as usize)
                    .unwrap_or(response.items.len()),
            );

            // https://docs.github.com/en/webhooks/webhook-events-and-payloads#workflow_run
            fn get_status(run: &Run) -> WorkflowStatus {
                match run.status.as_str() {
                    "completed" => match run.conclusion.as_deref() {
                        Some("success") => WorkflowStatus::Success,
                        Some(_) => WorkflowStatus::Failure,
                        None => {
                            tracing::warn!("Received completed status with empty conclusion for workflow run {}", run.id);
                            WorkflowStatus::Failure
                        }
                    },
                    _ => WorkflowStatus::Pending
                }
            }

            let mut stream = std::pin::pin!(response.into_stream(&self.client));
            while let Some(run) = stream.try_next().await? {
                let status = get_status(&run);
                let duration = match status {
                    WorkflowStatus::Pending => None,
                    WorkflowStatus::Success |
                    WorkflowStatus::Failure => {
                        let start = run.created_at;
                        let end = run.updated_at;
                        (end - start).to_std().ok()
                    }
                };

                let run = WorkflowRun {
                    id: run.id,
                    name: run.name,
                    url: run.html_url.to_string(),
                    status,
                    created_at: run.created_at,
                    duration
                };
                runs.push(run);
            }
            anyhow::Ok(runs)
        })
            .await?;
        Ok(runs)
    }

    /// Find all jobs for the latest execution of a workflow run with the given ID.
    pub async fn get_jobs_for_workflow_run(&self, run_id: RunId) -> anyhow::Result<Vec<Job>> {
        let jobs = perform_retryable(
            "get_jobs_for_workflow_run",
            RetryMethod::no_retry(),
            || async {
                let response = self
                    .client
                    .workflows(&self.repo_name.owner, &self.repo_name.name)
                    .list_jobs(run_id)
                    .per_page(100)
                    .send()
                    .await?;

                let mut jobs = Vec::with_capacity(
                    response
                        .total_count
                        .map(|v| v as usize)
                        .unwrap_or(response.items.len()),
                );
                let mut stream = std::pin::pin!(response.into_stream(&self.client));
                while let Some(job) = stream.try_next().await? {
                    jobs.push(job);
                }
                anyhow::Ok(jobs)
            },
        )
        .await?;
        Ok(jobs)
    }

    /// Cancels Github Actions workflows.
    pub async fn cancel_workflows(&self, run_ids: &[RunId]) -> anyhow::Result<()> {
        perform_retryable("cancel_workflows", RetryMethod::no_retry(), || async {
            let actions = self.client.actions();

            // Cancel all workflows in parallel
            futures::future::join_all(run_ids.iter().map(|run_id| {
                actions.cancel_workflow_run(self.repo_name.owner(), self.repo_name.name(), *run_id)
            }))
            .await
            .into_iter()
            .collect::<Result<Vec<_>, _>>()?;

            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    /// Add a set of labels to a PR.
    pub async fn add_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        perform_retryable("add_labels", RetryMethod::default(), || async {
            if !labels.is_empty() {
                let client = self
                    .client
                    .issues(self.repository().owner(), self.repository().name());
                client
                    .add_labels(pr.0, labels)
                    .await
                    .context("Cannot add label(s) to PR")?;
            }

            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    /// Remove a set of labels from a PR.
    pub async fn remove_labels(
        &self,
        pr: PullRequestNumber,
        labels: &[String],
    ) -> anyhow::Result<()> {
        perform_retryable("remove_labels", RetryMethod::default(), || async {
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
                        octocrab::Error::GitHub { source, .. }
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
            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    pub async fn fetch_nonclosed_pull_requests(&self) -> anyhow::Result<Vec<PullRequest>> {
        let prs = perform_retryable(
            "fetch_nonclosed_pull_requests",
            // This operation can take a long time
            RetryMethod::default().with_timeout(Duration::from_secs(40)),
            || async {
                let stream = self
                    .client
                    .pulls(self.repo_name.owner(), self.repo_name.name())
                    .list()
                    .state(octocrab::params::State::Open)
                    .per_page(100)
                    .send()
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!("Could not fetch PRs from {}: {error:?}", self.repo_name)
                    })?
                    .into_stream(&self.client);

                let mut stream = std::pin::pin!(stream);
                let mut prs = Vec::new();
                while let Some(pr) = stream.try_next().await? {
                    prs.push(pr.into());
                }
                anyhow::Ok(prs)
            },
        )
        .await?;
        Ok(prs)
    }

    async fn get_request<T: DeserializeOwned + Debug>(&self, path: &str) -> anyhow::Result<T> {
        let url = format!(
            "/repos/{}/{}/{path}",
            self.repo_name.owner(),
            self.repo_name.name(),
        );
        tracing::debug!("Sending GET request to {url}");
        let response: T = self.client.get(url.as_str(), None::<&()>).await?;
        tracing::debug!("Received response: {response:?}");
        Ok(response)
    }

    fn format_pr(&self, pr: PullRequestNumber) -> String {
        format!("{}/{}", self.repository(), pr)
    }
}

/// GraphQL APIs
impl GithubRepositoryClient {
    async fn graphql<T, V>(&self, query: &str, variables: V) -> anyhow::Result<T>
    where
        T: DeserializeOwned,
        V: Serialize,
    {
        #[derive(Serialize)]
        struct Payload<'a, V> {
            query: &'a str,
            variables: V,
        }

        #[derive(serde::Deserialize, Debug)]
        struct Error {
            #[allow(unused)]
            message: String,
        }

        #[derive(serde::Deserialize)]
        struct RawResponse<T> {
            errors: Option<Vec<Error>>,
            #[serde(flatten)]
            result: T,
        }

        let response = self
            .client
            .graphql::<RawResponse<T>>(&Payload { query, variables })
            .await
            .context("GraphQL request failed")?;

        let errors = response.errors.unwrap_or_default();
        if !errors.is_empty() {
            Err(anyhow::anyhow!("Query ended with error(s): {errors:?}"))
        } else {
            Ok(response.result)
        }
    }

    /// Hides a comment on an Issue, Commit, Pull Request, or Gist.
    ///
    /// GitHub Docs: <https://docs.github.com/en/graphql/reference/mutations#minimizecomment>
    pub async fn hide_comment(
        &self,
        node_id: &str,
        reason: HideCommentReason,
    ) -> anyhow::Result<()> {
        const QUERY: &str = "mutation($node_id: ID!, $reason: ReportedContentClassifiers!) {
            minimizeComment(input: {subjectId: $node_id, classifier: $reason}) {
                __typename
            }
        }";

        #[derive(Serialize)]
        struct Variables<'a> {
            node_id: &'a str,
            reason: HideCommentReason,
        }

        #[derive(Deserialize)]
        struct Output {}

        tracing::debug!(node_id, ?reason, "Hiding comment");
        perform_retryable("hide_comment", RetryMethod::default(), || async {
            self.graphql::<Output, Variables>(QUERY, Variables { node_id, reason })
                .await
                .context("Failed to hide comment")?;
            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    pub async fn get_comment_content(&self, node_id: &str) -> anyhow::Result<String> {
        const QUERY: &str = r#"
            query($node_id: ID!) {
                node(id: $node_id) {
                    ... on IssueComment {
                        body
                    }
                }
            }
        "#;

        #[derive(Serialize)]
        struct Variables<'a> {
            node_id: &'a str,
        }

        #[derive(Deserialize)]
        struct Output {
            data: OutputInner,
        }

        #[derive(Deserialize)]
        struct OutputInner {
            node: Option<IssueCommentNode>,
        }

        #[derive(Deserialize)]
        struct IssueCommentNode {
            body: String,
        }

        tracing::debug!(node_id, "Fetching comment content");

        let output = perform_retryable("get_comment_content", RetryMethod::default(), || async {
            self.graphql::<Output, Variables>(QUERY, Variables { node_id })
                .await
                .context("Failed to fetch comment content")
        })
        .await?;

        match output.data.node {
            Some(comment) => Ok(comment.body),
            None => anyhow::bail!("No comment found for node_id: {node_id}"),
        }
    }
    pub async fn update_comment_content(
        &self,
        node_id: &str,
        new_body: &str,
    ) -> anyhow::Result<()> {
        const QUERY: &str = r#"
            mutation($id: ID!, $body: String!) {
                updateIssueComment(input: {id: $id, body: $body}) {
                    issueComment {
                        id
                    }
                }
            }
        "#;

        #[derive(Serialize)]
        struct Variables<'a> {
            id: &'a str,
            body: &'a str,
        }

        tracing::debug!(node_id, "Updating comment content");

        perform_retryable("update_comment_content", RetryMethod::default(), || async {
            self.graphql::<(), Variables>(
                QUERY,
                Variables {
                    id: node_id,
                    body: new_body,
                },
            )
            .await
            .context("Failed to update comment")
        })
        .await?;
        Ok(())
    }
}

/// The reasons a piece of content can be reported or hidden.
///
/// GitHub Docs: <https://docs.github.com/en/graphql/reference/enums#reportedcontentclassifiers>
#[derive(Debug, Copy, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HideCommentReason {
    /// An abusive or harassing piece of content.
    Abuse,
    /// A duplicated piece of content.
    Duplicate,
    /// An irrelevant piece of content.
    OffTopic,
    /// An outdated piece of content.
    Outdated,
    /// The content has been resolved.
    Resolved,
    /// A spammy piece of content.
    Spam,
}

/// We have our own version to make it `Clone`, in order for retried requests to work.
#[derive(serde::Serialize, Clone)]
pub struct CheckRunOutput {
    pub title: String,
    pub summary: String,
}

#[cfg(test)]
mod tests {
    use crate::github::GithubRepoName;
    use crate::github::api::load_repositories;
    use crate::permissions::PermissionType;
    use crate::tests::ExternalHttpMock;
    use crate::tests::Repo;
    use crate::tests::{GitHub, User};
    use octocrab::models::UserId;
    use parking_lot::Mutex;
    use std::sync::Arc;

    #[tokio::test]
    async fn load_installed_repos() {
        let mock = ExternalHttpMock::start(Arc::new(Mutex::new(
            GitHub::default()
                .with_repo(
                    Repo::new(User::new(2, "foo"), "bar")
                        .with_user_perms(User::new(1, "user"), &[PermissionType::Try]),
                )
                .with_repo(Repo::new(User::new(2, "foo"), "baz")),
        )))
        .await;
        let client = mock.github_client();
        let team_api_client = mock.team_api_client();
        let mut repos = load_repositories(&client, &team_api_client).await.unwrap();
        assert_eq!(repos.len(), 3);

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
