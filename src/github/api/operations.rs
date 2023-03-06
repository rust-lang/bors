use reqwest::StatusCode;

use crate::github::api::client::GithubRepositoryClient;

#[derive(Debug)]
pub enum MergeError {
    NotFound,
    Conflict,
    AlreadyMerged,
    Unknown { status: StatusCode, text: String },
    NetworkError(octocrab::Error),
}

#[derive(serde::Serialize)]
struct MergeRequest<'a, 'b, 'c> {
    base: &'a str,
    head: &'b str,
    commit_message: &'c str,
}

/// Creates a merge commit on the given repository.
///
/// Documentation: https://docs.github.com/en/rest/branches/branches?apiVersion=2022-11-28#merge-a-branch
pub async fn merge_branches(
    repo: &GithubRepositoryClient,
    base_ref: &str,
    head_ref: &str,
    commit_message: &str,
) -> Result<(), MergeError> {
    let client = repo.client();
    let merge_url = repo
        .repository
        .merges_url
        .as_ref()
        .map(|url| url.to_string())
        .unwrap_or_else(|| {
            format!(
                "{}/repos/{}/{}/merges",
                client.base_url,
                repo.name().owner,
                repo.name().name
            )
        });

    let request = MergeRequest {
        base: base_ref,
        head: head_ref,
        commit_message,
    };
    let response = client._post(merge_url, Some(&request)).await;

    match response {
        Ok(response) => {
            let status = response.status();
            let text = response.text().await.unwrap_or_else(|_| String::new());

            log::debug!(
                "Response from merging `{head_ref}` into `{base_ref}` in `{}`: {status} ({text})",
                repo.name(),
            );

            match status {
                StatusCode::OK => Ok(()),
                StatusCode::NOT_FOUND => Err(MergeError::NotFound),
                StatusCode::CONFLICT => Err(MergeError::Conflict),
                StatusCode::NO_CONTENT => Err(MergeError::AlreadyMerged),
                _ => Err(MergeError::Unknown { status, text }),
            }
        }
        Err(error) => {
            log::debug!(
                "Merging `{head_ref}` into `{base_ref}` in `{}` failed: {error:?}",
                repo.name()
            );
            Err(MergeError::NetworkError(error))
        }
    }
}
