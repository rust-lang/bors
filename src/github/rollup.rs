use super::GithubRepoName;
use super::error::AppError;
use crate::server::ServerStateRef;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use octocrab::OctocrabBuilder;
use octocrab::params::repos::Reference;
use rand::{Rng, distr::Alphanumeric};
use std::collections::HashMap;
use tracing::Instrument;

/// Query parameters received from GitHub's OAuth callback.
///
/// Documentation: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#2-users-are-redirected-back-to-your-site-by-github
#[derive(serde::Deserialize)]
pub struct OAuthCallbackQuery {
    /// Temporary code from GitHub to exchange for an access token (expires in 10m).
    pub code: String,
    /// State passed in the initial OAuth request - contains rollup info created from the queue page.
    pub state: String,
}

#[derive(serde::Deserialize)]
pub struct OAuthState {
    pub pr_nums: Vec<u32>,
    pub repo_name: String,
    pub repo_owner: String,
}

pub async fn oauth_callback_handler(
    Query(callback): Query<OAuthCallbackQuery>,
    State(state): State<ServerStateRef>,
) -> Result<impl IntoResponse, AppError> {
    let oauth_config = state.oauth.as_ref().ok_or_else(|| {
        let error =
            anyhow::anyhow!("OAuth not configured. Please set CLIENT_ID and CLIENT_SECRET.");
        tracing::error!("{error}");
        error
    })?;

    let oauth_state: OAuthState = serde_json::from_str(&callback.state)
        .map_err(|_| anyhow::anyhow!("Invalid state parameter"))?;

    tracing::info!("Exchanging OAuth code for access token");
    let client = reqwest::Client::new();
    let token_response = client
        .post("https://github.com/login/oauth/access_token")
        .form(&[
            ("client_id", oauth_config.client_id()),
            ("client_secret", oauth_config.client_secret()),
            ("code", &callback.code),
        ])
        .send()
        .await
        .context("Failed to send OAuth token exchange request to GitHub")?
        .text()
        .await
        .context("Failed to read OAuth token response from GitHub")?;

    tracing::debug!("Extracting access token from OAuth response");
    let oauth_token_params: HashMap<String, String> =
        url::form_urlencoded::parse(token_response.as_bytes())
            .into_owned()
            .collect();
    let access_token = oauth_token_params
        .get("access_token")
        .ok_or_else(|| anyhow::anyhow!("No access token in response"))?;

    tracing::info!("Retrieved OAuth access token, creating rollup");

    let span = tracing::info_span!(
        "create_rollup",
        repo = %format!("{}/{}", oauth_state.repo_owner, oauth_state.repo_name),
        pr_nums = ?oauth_state.pr_nums
    );

    match create_rollup(state, oauth_state, access_token)
        .instrument(span)
        .await
    {
        Ok(pr_url) => {
            tracing::info!("Rollup created successfully, redirecting to: {pr_url}");
            Ok(Redirect::temporary(&pr_url).into_response())
        }
        Err(error) => {
            tracing::error!("Failed to create rollup: {error}");
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create rollup: {error}"),
            )
                .into_response())
        }
    }
}

/// Creates a rollup PR by merging multiple approved PRs into a single branch
/// in the user's fork, then opens a PR to the upstream repository.
async fn create_rollup(
    state: ServerStateRef,
    oauth_state: OAuthState,
    access_token: &str,
) -> anyhow::Result<String> {
    let OAuthState {
        repo_name,
        repo_owner,
        pr_nums,
    } = oauth_state;

    let gh_client = OctocrabBuilder::new()
        .user_access_token(access_token.to_string())
        .build()?;
    let user = gh_client.current().user().await?;
    let username = user.login;

    tracing::info!("User {username} is creating a rollup with PRs: {pr_nums:?}");

    // Ensure user has a fork
    match gh_client.repos(&username, &repo_name).get().await {
        Ok(repo) => repo,
        Err(_) => {
            anyhow::bail!(
                "You must have a fork of {username}/{repo_name} named {repo_name} under your account",
            );
        }
    };

    // Validate PRs
    let mut rollup_prs = Vec::new();
    for num in pr_nums {
        match state
            .db
            .get_pull_request(
                &GithubRepoName::new(&repo_owner, &repo_name),
                (num as u64).into(),
            )
            .await?
        {
            Some(pr) => {
                if !pr.is_rollupable() {
                    let error = format!("PR #{num} cannot be included in rollup");
                    tracing::error!("{error}");
                    anyhow::bail!(error);
                }
                rollup_prs.push(pr);
            }
            None => anyhow::bail!("PR #{num} not found"),
        }
    }

    if rollup_prs.is_empty() {
        anyhow::bail!("No pull requests are marked for rollup");
    }

    // Sort PRs by number
    rollup_prs.sort_by_key(|pr| pr.number.0);

    // Fetch the first PR from GitHub to determine the target base branch
    let first_pr_github = gh_client
        .pulls(&repo_owner, &repo_name)
        .get(rollup_prs[0].number.0)
        .await?;
    let base_ref = first_pr_github.base.ref_field.clone();

    // Fetch the current SHA of the base branch - this is the commit our
    // rollup branch starts from.
    let base_branch_ref = gh_client
        .repos(&repo_owner, &repo_name)
        .get_ref(&Reference::Branch(base_ref.clone()))
        .await?;
    let base_sha = match base_branch_ref.object {
        octocrab::models::repos::Object::Commit { sha, .. } => sha,
        octocrab::models::repos::Object::Tag { sha, .. } => sha,
        _ => unreachable!(),
    };

    let branch_suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let branch_name = format!("rollup-{branch_suffix}");

    // Create the branch on the user's fork
    gh_client
        .repos(&username, &repo_name)
        .create_ref(
            &octocrab::params::repos::Reference::Branch(branch_name.clone()),
            base_sha,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not create rollup branch {branch_name}: {error}",)
        })?;

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    // Merge each PR's commits into the rollup branch
    for pr in rollup_prs {
        let pr_github = gh_client
            .pulls(&repo_owner, &repo_name)
            .get(pr.number.0)
            .await?;

        // Skip PRs that don't target the same base branch
        if pr_github.base.ref_field != base_ref {
            failures.push(pr);
            continue;
        }

        let head_sha = pr_github.head.sha.clone();
        let merge_msg = format!(
            "Rollup merge of #{} - {}, r={}\n\n{}\n\n{}",
            pr.number.0,
            pr_github.head.ref_field,
            pr.approver().unwrap_or("unknown"),
            pr.title,
            &pr_github.body.unwrap_or_default()
        );

        // Merge the PR's head commit into the rollup branch
        let merge_attempt = gh_client
            .repos(&username, &repo_name)
            .merge(&head_sha, &branch_name)
            .commit_message(&merge_msg)
            .send()
            .await;

        match merge_attempt {
            Ok(_) => {
                successes.push(pr);
            }
            Err(error) => {
                if let octocrab::Error::GitHub { source, .. } = &error {
                    if source.status_code == http::StatusCode::CONFLICT {
                        failures.push(pr);
                        continue;
                    }

                    anyhow::bail!(
                        "Merge failed with GitHub error (status {}): {}",
                        source.status_code,
                        source.message
                    );
                }

                anyhow::bail!("Merge failed with unexpected error: {error}");
            }
        }
    }

    let mut body = "Successful merges:\n\n".to_string();
    for pr in &successes {
        body.push_str(&format!(" - #{} ({})\n", pr.number.0, pr.title));
    }

    if !failures.is_empty() {
        body.push_str("\nFailed merges:\n\n");
        for pr in &failures {
            body.push_str(&format!(" - #{} ({})\n", pr.number.0, pr.title));
        }
    }
    body.push_str("\nr? @ghost\n@rustbot modify labels: rollup");

    let title = format!("Rollup of {} pull requests", successes.len());

    // Create the rollup PR from the user's fork branch to the base branch
    let pr = gh_client
        .pulls(&repo_owner, &repo_name)
        .create(&title, format!("{username}:{branch_name}"), &base_ref)
        .body(&body)
        .send()
        .await?;
    let pr_url = pr
        .html_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("GitHub returned PR without html_url"))?
        .to_string();

    Ok(pr_url)
}
