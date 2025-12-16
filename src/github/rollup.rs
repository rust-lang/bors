use super::GithubRepoName;
use super::error::AppError;
use crate::PgDbClient;
use crate::github::api::client::GithubRepositoryClient;
use crate::github::oauth::OAuthClient;
use crate::server::ServerStateRef;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use octocrab::Octocrab;
use rand::{Rng, distr::Alphanumeric};
use std::sync::Arc;
use tracing::Instrument;

/// Query parameters received from GitHub's OAuth callback.
///
/// Documentation: https://docs.github.com/en/apps/oauth-apps/building-oauth-apps/authorizing-oauth-apps#2-users-are-redirected-back-to-your-site-by-github
#[derive(serde::Deserialize)]
pub struct OAuthCallbackQuery {
    /// Temporary code from GitHub to exchange for an access token (expires in 10m).
    code: String,
    /// State passed in the initial OAuth request - contains rollup info created from the queue page.
    state: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct OAuthRollupState {
    pr_nums: Vec<u32>,
    repo_name: String,
    repo_owner: String,
}

pub async fn oauth_callback_handler(
    State(db): State<Arc<PgDbClient>>,
    State(oauth): State<Option<OAuthClient>>,
    State(ServerStateRef(state)): State<ServerStateRef>,
    Query(callback): Query<OAuthCallbackQuery>,
) -> Result<impl IntoResponse, AppError> {
    let Some(oauth_client) = oauth else {
        let error =
            anyhow::anyhow!("OAuth not configured. Please set CLIENT_ID and CLIENT_SECRET.");
        tracing::error!("{error}");
        return Err(error.into());
    };

    let oauth_state: OAuthRollupState = serde_json::from_str(&callback.state)
        .map_err(|_| anyhow::anyhow!("Invalid state parameter"))?;

    let repo_name = GithubRepoName::new(&oauth_state.repo_owner, &oauth_state.repo_name);
    let Some(repo_state) = state.get_repo(&repo_name) else {
        return Err(anyhow::anyhow!("Repository {repo_name} not found").into());
    };

    let user_client = oauth_client
        .get_authenticated_client(&callback.code)
        .await?;

    let span = tracing::info_span!(
        "create_rollup",
        repo = %repo_name,
        pr_nums = ?oauth_state.pr_nums
    );

    match create_rollup(db, oauth_state, &repo_state.client, user_client)
        .instrument(span)
        .await
    {
        Ok(pr_url) => {
            tracing::info!("Rollup created successfully, redirecting to: {pr_url}");
            Ok(Redirect::temporary(&pr_url).into_response())
        }
        Err(error) => {
            tracing::error!("Failed to create rollup: {error:?}");
            Ok((
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("Failed to create rollup: {error:?}"),
            )
                .into_response())
        }
    }
}

/// Creates a rollup PR by merging multiple approved PRs into a single branch
/// in the user's fork, then opens a PR to the upstream repository.
async fn create_rollup(
    db: Arc<PgDbClient>,
    rollup_state: OAuthRollupState,
    gh_client: &GithubRepositoryClient,
    user_client: Octocrab,
) -> anyhow::Result<String> {
    let OAuthRollupState {
        repo_name,
        repo_owner,
        pr_nums,
    } = rollup_state;

    let user = user_client
        .current()
        .user()
        .await
        .context("Cannot get user authenticated with OAuth")?;
    let username = user.login;

    tracing::info!("User {username} is creating a rollup with PRs: {pr_nums:?}");

    // Ensure user has a fork
    match user_client.repos(&username, &repo_name).get().await {
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
        match db
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
    let first_pr_github = gh_client.get_pull_request(rollup_prs[0].number).await?;
    let base_branch = &first_pr_github.base.name;

    // Fetch the current SHA of the base branch - this is the commit our
    // rollup branch starts from.
    let base_branch_sha = gh_client.get_branch_sha(&base_branch).await?;

    let branch_suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let branch_name = format!("rollup-{branch_suffix}");

    // Create the branch on the user's fork
    user_client
        .repos(&username, &repo_name)
        .create_ref(
            &octocrab::params::repos::Reference::Branch(branch_name.clone()),
            base_branch_sha.0,
        )
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not create rollup branch {branch_name}: {error}",)
        })?;

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    // Merge each PR's commits into the rollup branch
    for pr in rollup_prs {
        let pr_github = gh_client.get_pull_request(pr.number).await?;

        // Skip PRs that don't target the same base branch
        if pr_github.base.name != *base_branch {
            failures.push(pr);
            continue;
        }

        let head_sha = pr_github.head.sha.clone();
        let merge_msg = format!(
            "Rollup merge of #{} - {}, r={}\n\n{}\n\n{}",
            pr.number.0,
            pr_github.head.name,
            pr.approver().unwrap_or("unknown"),
            pr.title,
            pr_github.message
        );

        // Merge the PR's head commit into the rollup branch
        let merge_attempt = user_client
            .repos(&username, &repo_name)
            .merge(&head_sha.0, &branch_name)
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
    let pr = user_client
        .pulls(&repo_owner, &repo_name)
        .create(&title, format!("{username}:{branch_name}"), base_branch)
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

#[cfg(test)]
mod tests {
    use crate::github::rollup::OAuthRollupState;
    use crate::github::{GithubRepoName, PullRequestNumber};
    use crate::tests::{ApiRequest, BorsTester, default_repo_name, run_test};
    use http::StatusCode;

    #[sqlx::test]
    async fn create_rollup_missing_fork(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr1 = tester.open_pr(default_repo_name(), |_| {}).await?;
            let pr2 = tester.open_pr(default_repo_name(), |_| {}).await?;

            let repo_name = default_repo_name();
            tester
                .api_request(rollup_request(
                    "rolluper",
                    repo_name.clone(),
                    &[pr1.number, pr2.number],
                ))
                .await?
                .assert_status(StatusCode::INTERNAL_SERVER_ERROR)
                .assert_body_contains(&format!(
                    "You must have a fork of rolluper/{} named borstest under your account",
                    repo_name.name()
                ));
            Ok(())
        })
        .await;
    }

    fn rollup_request(code: &str, repo: GithubRepoName, prs: &[PullRequestNumber]) -> ApiRequest {
        let state = OAuthRollupState {
            pr_nums: prs.iter().map(|v| v.0 as u32).collect(),
            repo_name: repo.name,
            repo_owner: repo.owner,
        };
        ApiRequest::get("/oauth/callback")
            .query("code", code)
            .query("state", &serde_json::to_string(&state).unwrap())
    }
}
