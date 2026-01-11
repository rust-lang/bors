use http::StatusCode;
use octocrab::models::CheckRunId;
use octocrab::models::checks::CheckRun;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use octocrab::params::repos::Reference;
use thiserror::Error;

use crate::github::api::client::{CheckRunOutput, CommitAuthor, GithubRepositoryClient};
use crate::github::{CommitSha, TreeSha};

#[derive(Copy, Clone, PartialEq, Eq)]
pub enum ForcePush {
    Yes,
    No,
}

#[derive(Error, Debug)]
pub enum MergeError {
    #[error("Branch not found")]
    NotFound,
    #[error("Merge conflict")]
    Conflict,
    #[error("Branch was already merged")]
    AlreadyMerged,
    #[error("Unknown error ({status}): {text}")]
    Unknown { status: StatusCode, text: String },
    #[error("Network error: {0}")]
    NetworkError(#[from] octocrab::Error),
    #[error("Request timed out")]
    Timeout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeResult {
    Success(Commit),
    Conflict,
}

#[derive(serde::Serialize)]
struct MergeRequest<'a, 'b, 'c> {
    base: &'a str,
    head: &'b str,
    commit_message: &'c str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub sha: CommitSha,
    pub parents: Vec<CommitSha>,
    pub tree: TreeSha,
}

/// Creates a merge commit on the given repository.
///
/// Documentation: https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#merge-a-branch
pub async fn merge_branches(
    repo: &GithubRepositoryClient,
    base_ref: &str,
    head_sha: &CommitSha,
    commit_message: &str,
) -> Result<Commit, MergeError> {
    let client = repo.client();
    let merge_url = format!("/repos/{}/merges", repo.repository());

    let request = MergeRequest {
        base: base_ref,
        head: head_sha.as_ref(),
        commit_message,
    };
    let response = client._post(merge_url, Some(&request)).await;

    #[derive(serde::Deserialize)]
    struct TreeResponse {
        sha: String,
    }

    #[derive(serde::Deserialize)]
    struct ParentResponse {
        sha: String,
    }

    #[derive(serde::Deserialize)]
    struct CommitResponse {
        tree: TreeResponse,
    }

    #[derive(serde::Deserialize)]
    struct MergeCommitResponse {
        sha: String,
        commit: CommitResponse,
        parents: Vec<ParentResponse>,
    }

    impl From<MergeCommitResponse> for Commit {
        fn from(response: MergeCommitResponse) -> Self {
            let sha: CommitSha = response.sha.into();
            let tree: TreeSha = response.commit.tree.sha.into();
            let parents = response
                .parents
                .into_iter()
                .map(|parent| CommitSha(parent.sha))
                .collect();
            Commit { sha, tree, parents }
        }
    }

    match response {
        Ok(response) => {
            let status = response.status();
            let text = client.body_to_string(response).await.unwrap_or_default();

            tracing::trace!(
                "Response from merging `{head_sha}` into `{base_ref}` in `{}`: {status} ({text})",
                repo.repository(),
            );

            match status {
                StatusCode::CREATED => {
                    let response: MergeCommitResponse =
                        serde_json::from_str(&text).map_err(|error| MergeError::Unknown {
                            status,
                            text: format!("{error:?}"),
                        })?;
                    Ok(response.into())
                }
                StatusCode::NOT_FOUND => Err(MergeError::NotFound),
                StatusCode::CONFLICT => {
                    // It seems that GitHub sometimes returns 409 even if the error is a
                    // "permission issue" (see https://github.com/rust-lang/rust/pull/150925#issuecomment-3734359600).
                    // We try to detect this situation, and treat it as a transient error.
                    #[derive(serde::Deserialize)]
                    struct MessageResponse {
                        message: String,
                    }
                    match serde_json::from_str::<MessageResponse>(&text) {
                        Ok(response) => {
                            if response
                                .message
                                .contains("not authorized to push to this branch")
                            {
                                tracing::error!(
                                    "Encountered transient merge permission error: `{text}`"
                                );
                                // In this case, it might be a transient GitHub error?
                                Err(MergeError::Unknown { status, text })
                            } else {
                                Err(MergeError::Conflict)
                            }
                        }
                        Err(error) => {
                            tracing::error!(
                                "Could not deserialize merge conflict response `{text}`: {error:?}"
                            );
                            Err(MergeError::Conflict)
                        }
                    }
                }
                StatusCode::NO_CONTENT => Err(MergeError::AlreadyMerged),
                _ => Err(MergeError::Unknown { status, text }),
            }
        }
        Err(error) => {
            tracing::debug!(
                "Merging `{head_sha}` into `{base_ref}` in `{}` failed: {error:?}",
                repo.repository()
            );
            Err(MergeError::NetworkError(error))
        }
    }
}

/// Forcefully updates the branch to the given commit `sha`.
/// If the branch does not exist yet, it instead attempts to create it.
pub async fn set_branch_to_commit(
    repo: &GithubRepositoryClient,
    branch_name: String,
    sha: &CommitSha,
    force: ForcePush,
) -> Result<(), BranchUpdateError> {
    // Fast-path: assume that the branch exists
    match update_branch(repo, branch_name.clone(), sha, force).await {
        Ok(_) => Ok(()),
        // Branch does not exist yet or there was some other error.
        // Try to create it instead if we are force pushing.
        Err(BranchUpdateError::ValidationFailed(_)) if force == ForcePush::Yes => {
            match create_branch(repo, branch_name.clone(), sha).await {
                Ok(_) => Ok(()),
                Err(error) => Err(BranchUpdateError::Custom(error)),
            }
        }
        Err(error) => Err(error),
    }
}

pub async fn create_branch(
    repo: &GithubRepositoryClient,
    name: String,
    sha: &CommitSha,
) -> Result<(), String> {
    repo.client()
        .repos(repo.repository().owner(), repo.repository().name())
        .create_ref(&Reference::Branch(name), sha.as_ref())
        .await
        .map_err(|error| format!("Cannot create branch: {error}"))?;
    Ok(())
}

#[derive(Error, Debug)]
pub enum BranchUpdateError {
    #[error("Conflict while updating branch {0}")]
    Conflict(String),
    #[error("Validation failed for branch {0}")]
    ValidationFailed(String),
    #[error("Request timed out")]
    Timeout,
    #[error("IO error")]
    OctocrabError(#[from] octocrab::Error),
    #[error("Unknown error: {0}")]
    Custom(String),
}

/// Force update the branch with the given `branch_name` to the given `sha`.
async fn update_branch(
    repo: &GithubRepositoryClient,
    branch_name: String,
    sha: &CommitSha,
    force: ForcePush,
) -> Result<(), BranchUpdateError> {
    let url = format!(
        "/repos/{}/git/refs/{}",
        repo.repository(),
        Reference::Branch(branch_name.clone()).ref_url()
    );

    tracing::debug!("Updating branch {} to SHA {}", url, sha.as_ref());

    #[derive(serde::Serialize)]
    struct Request<'a> {
        sha: &'a str,
        force: bool,
    }

    let res = repo
        .client()
        ._patch(
            url.as_str(),
            Some(&Request {
                sha: sha.as_ref(),
                force: matches!(force, ForcePush::Yes),
            }),
        )
        .await?;

    let status = res.status();
    tracing::trace!(
        "Updating branch response: status={}, text={:?}",
        status,
        repo.client().body_to_string(res).await
    );

    match status {
        StatusCode::OK => Ok(()),
        StatusCode::CONFLICT => Err(BranchUpdateError::Conflict(branch_name)),
        StatusCode::UNPROCESSABLE_ENTITY => Err(BranchUpdateError::ValidationFailed(branch_name)),
        _ => Err(BranchUpdateError::Custom(format!(
            "Unexpected status {status} for branch {branch_name}",
        ))),
    }
}

#[derive(Error, Debug)]
pub enum CommitCreateError {
    #[error("Conflict while creating a commit")]
    Conflict,
    #[error("Validation failed for creating commit")]
    ValidationFailed,
    #[error("Tree or parents not found")]
    TreeOrParentsNotFound,
    #[error("Request timed out")]
    Timeout,
    #[error("IO error")]
    OctocrabError(#[from] octocrab::Error),
    #[error("Unknown error: {0}")]
    Custom(String),
}

/// Create a new commit with the given tree SHA and author.
/// https://docs.github.com/en/rest/git/commits?apiVersion=2022-11-28#create-a-commit
pub(super) async fn create_commit(
    repo: &GithubRepositoryClient,
    tree: &TreeSha,
    parents: &[CommitSha],
    message: &str,
    author: &CommitAuthor,
) -> Result<CommitSha, CommitCreateError> {
    let url = format!("/repos/{}/git/commits", repo.repository(),);

    tracing::debug!("Creating commit with tree {tree}, message {message} and author {author:?}");

    #[derive(serde::Serialize)]
    struct Author<'a> {
        name: &'a str,
        email: &'a str,
    }

    #[derive(serde::Serialize)]
    struct Request<'a> {
        message: &'a str,
        tree: &'a str,
        parents: &'a [&'a str],
        author: Author<'a>,
    }

    let parents = parents.iter().map(|sha| sha.as_ref()).collect::<Vec<_>>();
    let res = repo
        .client()
        ._post(
            url.as_str(),
            Some(&Request {
                message,
                tree: tree.as_ref(),
                parents: &parents,
                author: Author {
                    name: &author.name,
                    email: &author.email,
                },
            }),
        )
        .await?;

    let status = res.status();
    let text = repo.client().body_to_string(res).await.unwrap_or_default();
    tracing::trace!("Creating commit response: status={status}, text={text}");

    #[derive(serde::Deserialize)]
    struct CreateCommitResponse {
        sha: String,
    }

    match status {
        StatusCode::CREATED => {
            let response: CreateCommitResponse = serde_json::from_str(&text).map_err(|error| {
                CommitCreateError::Custom(format!(
                    "Cannot deserialize create commit response: {error:?}"
                ))
            })?;
            Ok(CommitSha(response.sha))
        }
        StatusCode::CONFLICT => Err(CommitCreateError::Conflict),
        StatusCode::UNPROCESSABLE_ENTITY => Err(CommitCreateError::ValidationFailed),
        StatusCode::NOT_FOUND => Err(CommitCreateError::TreeOrParentsNotFound),
        _ => Err(CommitCreateError::Custom(format!(
            "Unexpected status {status} for creating a commit with message {message}",
        ))),
    }
}

pub async fn create_check_run(
    repo: &GithubRepositoryClient,
    name: &str,
    head_sha: &CommitSha,
    status: CheckRunStatus,
    output: CheckRunOutput,
    external_id: &str,
) -> Result<CheckRun, octocrab::Error> {
    repo.client()
        .checks(repo.repository().owner(), repo.repository().name())
        .create_check_run(name, head_sha.to_string())
        .external_id(external_id)
        .status(status)
        .output(output.into())
        .send()
        .await
}

impl From<CheckRunOutput> for octocrab::params::checks::CheckRunOutput {
    fn from(value: CheckRunOutput) -> Self {
        let CheckRunOutput { title, summary } = value;
        Self {
            title,
            summary,
            text: None,
            annotations: vec![],
            images: vec![],
        }
    }
}

pub async fn update_check_run(
    repo: &GithubRepositoryClient,
    check_run_id: CheckRunId,
    status: CheckRunStatus,
    conclusion: Option<CheckRunConclusion>,
) -> Result<CheckRun, octocrab::Error> {
    let checks = repo
        .client()
        .checks(repo.repository().owner(), repo.repository().name());

    let mut request = checks.update_check_run(check_run_id).status(status);

    if let Some(conclusion) = conclusion {
        request = request.conclusion(conclusion);
    }

    request.send().await
}

/// Attempts to merge a head commit into a base commit using a specified branch.
pub async fn attempt_merge(
    client: &GithubRepositoryClient,
    branch_name: &str,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
    merge_message: &str,
) -> anyhow::Result<MergeResult> {
    tracing::debug!(
        "Attempting to merge {head_sha} into base SHA {base_sha} using branch {branch_name}"
    );

    // Reset the merge branch to point to base branch
    client
        .set_branch_to_sha(branch_name, base_sha, ForcePush::Yes)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Cannot set merge branch {branch_name} to {base_sha}: {error:?}",)
        })?;

    // then merge PR head commit into the merge branch.
    match client
        .merge_branches(branch_name, head_sha, merge_message)
        .await
    {
        Ok(merged_commit) => {
            tracing::debug!("Merge successful, SHA: {}", merged_commit.sha);
            Ok(MergeResult::Success(merged_commit))
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");
            Ok(MergeResult::Conflict)
        }
        Err(error) => Err(error.into()),
    }
}
