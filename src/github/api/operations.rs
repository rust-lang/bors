use http::StatusCode;
use octocrab::models::CheckRunId;
use octocrab::models::checks::CheckRun;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use octocrab::params::repos::Reference;
use thiserror::Error;

use crate::github::CommitSha;
use crate::github::api::client::{CheckRunOutput, GithubRepositoryClient};

#[derive(Copy, Clone)]
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

#[derive(serde::Serialize)]
struct MergeRequest<'a, 'b, 'c> {
    base: &'a str,
    head: &'b str,
    commit_message: &'c str,
}

#[derive(serde::Deserialize)]
struct MergeResponse {
    sha: String,
}

/// Creates a merge commit on the given repository.
///
/// Documentation: https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#merge-a-branch
pub async fn merge_branches(
    repo: &GithubRepositoryClient,
    base_ref: &str,
    head_sha: &CommitSha,
    commit_message: &str,
) -> Result<CommitSha, MergeError> {
    let client = repo.client();
    let merge_url = format!("/repos/{}/merges", repo.repository());

    let request = MergeRequest {
        base: base_ref,
        head: head_sha.as_ref(),
        commit_message,
    };
    let response = client._post(merge_url, Some(&request)).await;

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
                    let response: MergeResponse =
                        serde_json::from_str(&text).map_err(|error| MergeError::Unknown {
                            status,
                            text: format!("{error:?}"),
                        })?;
                    let sha: CommitSha = response.sha.into();
                    Ok(sha)
                }
                StatusCode::NOT_FOUND => Err(MergeError::NotFound),
                StatusCode::CONFLICT => Err(MergeError::Conflict),
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
        Err(BranchUpdateError::BranchNotFound(_)) => {
            // Branch does not exist yet, try to create it
            match create_branch(repo, branch_name.clone(), sha).await {
                Ok(_) => Ok(()),
                Err(error) => Err(BranchUpdateError::Custom(error)),
            }
        }
        Err(error) => Err(error),
    }
}

async fn create_branch(
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
    #[error("Branch {0} was not found")]
    BranchNotFound(String),
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

    let res = repo
        .client()
        ._patch(
            url.as_str(),
            Some(&serde_json::json!({
                "sha": sha.as_ref(),
                "force": matches!(force, ForcePush::Yes)
            })),
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
        StatusCode::NOT_FOUND => Err(BranchUpdateError::BranchNotFound(branch_name)),
        StatusCode::CONFLICT => Err(BranchUpdateError::Conflict(branch_name)),
        StatusCode::UNPROCESSABLE_ENTITY => Err(BranchUpdateError::ValidationFailed(branch_name)),
        _ => Err(BranchUpdateError::Custom(format!(
            "Unexpected status {status} for branch {branch_name}",
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
