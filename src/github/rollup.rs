use super::{GithubRepoName, PullRequest, PullRequestNumber};
use crate::PgDbClient;
use crate::github::api::client::GithubRepositoryClient;
use crate::github::api::operations::{ForcePush, MergeError};
use crate::github::oauth::{OAuthClient, UserGitHubClient};
use crate::server::ServerStateRef;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
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

#[derive(Debug)]
pub enum RollupError {
    BaseRepoNotFound { repo_name: GithubRepoName },
    ForkNotFound { repo_name: GithubRepoName },
    Generic(anyhow::Error),
    PullRequestNotRollupable { pr: PullRequestNumber },
    PullRequestNotFound { pr: PullRequestNumber },
    NoPullRequestsSelected,
}

impl IntoResponse for RollupError {
    fn into_response(self) -> Response {
        match self {
            RollupError::Generic(error) => (StatusCode::INTERNAL_SERVER_ERROR, error.to_string()),
            RollupError::BaseRepoNotFound { repo_name } => (
                StatusCode::BAD_REQUEST,
                format!("Repository {repo_name} was not found"),
            ),
            RollupError::ForkNotFound { repo_name } => (
                StatusCode::BAD_REQUEST,
                format!("Fork {repo_name} not found, create it before opening the rollup"),
            ),
            RollupError::PullRequestNotRollupable { pr } => (
                StatusCode::BAD_REQUEST,
                format!("Pull request #{pr} cannot be included in a rollup"),
            ),
            RollupError::PullRequestNotFound { pr } => (
                StatusCode::BAD_REQUEST,
                format!("Pull request #{pr} was not found"),
            ),
            RollupError::NoPullRequestsSelected => (
                StatusCode::BAD_REQUEST,
                "No pull requests were selected".to_string(),
            ),
        }
        .into_response()
    }
}

impl<E> From<E> for RollupError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self::Generic(err.into())
    }
}

pub async fn oauth_callback_handler(
    State(db): State<Arc<PgDbClient>>,
    State(oauth): State<Option<OAuthClient>>,
    State(ServerStateRef(state)): State<ServerStateRef>,
    Query(callback): Query<OAuthCallbackQuery>,
) -> Result<impl IntoResponse, RollupError> {
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
        return Err(RollupError::ForkNotFound { repo_name });
    };

    let user_client = oauth_client
        .get_authenticated_client(repo_name.clone(), &callback.code)
        .await?;

    let span = tracing::info_span!(
        "create_rollup",
        repo = %repo_name,
        pr_nums = ?oauth_state.pr_nums
    );

    let pr = match create_rollup(db, oauth_state, &repo_state.client, user_client)
        .instrument(span.clone())
        .await
    {
        Ok(pr) => pr,
        Err(err) => {
            tracing::error!("Failed to create rollup: {err:?}");
            return Err(err);
        }
    };

    let _guard = span.enter();
    let pr_url = pr
        .html_url
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("GitHub returned PR without html_url"))?
        .to_string();

    tracing::info!("Rollup created successfully, redirecting to: {pr_url}");
    Ok(Redirect::temporary(&pr_url).into_response())
}

/// Creates a rollup PR by merging multiple approved PRs into a single branch
/// in the user's fork, then opens a PR to the upstream repository.
async fn create_rollup(
    db: Arc<PgDbClient>,
    rollup_state: OAuthRollupState,
    gh_client: &GithubRepositoryClient,
    user_client: UserGitHubClient,
) -> Result<PullRequest, RollupError> {
    let OAuthRollupState {
        repo_name,
        repo_owner,
        pr_nums,
    } = rollup_state;

    let username = user_client.username;

    tracing::info!("User {username} is creating a rollup with PRs: {pr_nums:?}");

    // Ensure user has a fork
    match user_client.client.get_repo().await {
        Ok(repo) => repo,
        Err(_) => {
            return Err(RollupError::BaseRepoNotFound {
                repo_name: user_client.client.repository().clone(),
            });
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
                    return Err(RollupError::PullRequestNotRollupable {
                        pr: PullRequestNumber(num as u64),
                    });
                }
                rollup_prs.push(pr);
            }
            None => {
                return Err(RollupError::PullRequestNotFound {
                    pr: PullRequestNumber(num as u64),
                });
            }
        }
    }

    if rollup_prs.is_empty() {
        return Err(RollupError::NoPullRequestsSelected);
    }

    // Sort PRs by priority and then PR number
    // We want to try to merge PRs with higher priority first, so that in case of conflicts they
    // get in, rather than being left out.
    rollup_prs.sort_by(|a, b| {
        a.priority
            .cmp(&b.priority)
            .reverse()
            .then(a.number.cmp(&b.number))
    });

    // Fetch the first PR from GitHub to determine the target base branch
    let first_pr_github = gh_client.get_pull_request(rollup_prs[0].number).await?;
    let base_branch = &first_pr_github.base.name;

    // Fetch the current SHA of the base branch - this is the commit our
    // rollup branch starts from.
    let base_branch_sha = gh_client.get_branch_sha(base_branch).await?;

    let branch_suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let rollup_branch = format!("rollup-{branch_suffix}");

    // Create the branch on the user's fork
    user_client
        .client
        .set_branch_to_sha(&rollup_branch, &base_branch_sha, ForcePush::Yes)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not create rollup branch {rollup_branch}: {error:?}")
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
            .client
            .merge_branches(&rollup_branch, &head_sha, &merge_msg)
            .await;

        match merge_attempt {
            Ok(_) => {
                successes.push(pr);
            }
            Err(error) => match error {
                MergeError::Conflict => {
                    failures.push(pr);
                }
                error => {
                    return Err(anyhow::anyhow!(
                        "Merge of #{} failed with error: {error:?}",
                        pr.number
                    )
                    .into());
                }
            },
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

    // Create the rollup PR from the user's fork branch to the main repo base branch
    let pr = gh_client
        .create_pr(
            &title,
            &format!("{username}:{rollup_branch}"),
            base_branch,
            &body,
        )
        .await
        .context("Cannot create PR")?;

    Ok(pr)
}

#[cfg(test)]
mod tests {
    use crate::github::rollup::OAuthRollupState;
    use crate::github::{GithubRepoName, PullRequestNumber};
    use crate::tests::{
        ApiRequest, ApiResponse, BorsTester, Comment, GitHub, MergeBehavior, PullRequest, Repo,
        User, default_repo_name, run_test,
    };
    use http::StatusCode;

    #[sqlx::test]
    async fn rollup_missing_fork(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            let pr1 = ctx.open_pr((), |_| {}).await?;
            let pr2 = ctx.open_pr((), |_| {}).await?;

            make_rollup(ctx, &[&pr1, &pr2])
                .await?
                .assert_status(StatusCode::BAD_REQUEST)
                .assert_body(&format!(
                    "Repository rolluper/{} was not found",
                    pr1.repo.name()
                ));
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_non_rollupable_pr(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr1 = ctx.open_pr((), |_| {}).await?;
            let pr2 = ctx.open_pr((), |_| {}).await?;

            make_rollup(ctx, &[&pr1, &pr2])
                .await?
                .assert_status(StatusCode::BAD_REQUEST)
                .assert_body(&format!(
                    "Pull request #{} cannot be included in a rollup",
                    pr1.number
                ));
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_nonexistent_pr(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            ctx.api_request(rollup_request(
                &rollup_user().name,
                default_repo_name(),
                &[50u64.into()],
            ))
            .await?
            .assert_status(StatusCode::BAD_REQUEST)
            .assert_body("Pull request #50 was not found");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_merge_conflict(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            ctx.modify_repo(fork_repo(), |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Fail the third merge
                    n != 3
                }));
            });
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            let pr4 = ctx.open_pr((), |_| {}).await?;
            let pr5 = ctx.open_pr((), |_| {}).await?;
            for pr in &[&pr2, &pr3, &pr4, &pr5] {
                ctx.approve(pr.id()).await?;
            }

            make_rollup(ctx, &[&pr2, &pr3, &pr4, &pr5])
                .await?
                .assert_status(StatusCode::TEMPORARY_REDIRECT);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(6).description, @r"
        Successful merges:

         - #2 (Title of PR 2)
         - #3 (Title of PR 3)
         - #5 (Title of PR 5)

        Failed merges:

         - #4 (Title of PR 4)

        r? @ghost
        @rustbot modify labels: rollup
        ");
    }

    #[sqlx::test]
    async fn rollup(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let pr_url = make_rollup(ctx, &[&pr2, &pr3])
                .await?
                .assert_status(StatusCode::TEMPORARY_REDIRECT)
                .get_header("location");
            insta::assert_snapshot!(pr_url, @"https://github.com/rust-lang/borstest/pull/4");
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(4).description, @r"
        Successful merges:

         - #2 (Title of PR 2)
         - #3 (Title of PR 3)

        r? @ghost
        @rustbot modify labels: rollup
        ");
    }

    #[sqlx::test]
    async fn rollup_order_by_priority(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr4 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr5 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;
            ctx.post_comment(Comment::new(pr3.id(), "@bors p=5"))
                .await?;
            ctx.approve(pr4.id()).await?;
            ctx.approve(pr4.id()).await?;
            ctx.post_comment(Comment::new(pr4.id(), "@bors p=1"))
                .await?;
            ctx.approve(pr5.id()).await?;

            make_rollup(ctx, &[&pr2, &pr3, &pr4, &pr5])
                .await?
                .assert_status(StatusCode::TEMPORARY_REDIRECT);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(6).description, @r"
        Successful merges:

         - #3 (Title of PR 3)
         - #4 (Title of PR 4)
         - #2 (Title of PR 2)
         - #5 (Title of PR 5)

        r? @ghost
        @rustbot modify labels: rollup
        ");
    }

    async fn make_rollup(
        ctx: &mut BorsTester,
        prs: &[&PullRequest],
    ) -> anyhow::Result<ApiResponse> {
        let prs = prs.iter().map(|pr| pr.number).collect::<Vec<_>>();
        ctx.api_request(rollup_request(
            &rollup_user().name,
            default_repo_name(),
            &prs,
        ))
        .await
    }

    fn rollup_state() -> GitHub {
        let mut gh = GitHub::default();
        let rolluper = rollup_user();
        gh.add_user(rolluper.clone());
        // Create fork
        let mut repo = Repo::new(rolluper, fork_repo().name());
        repo.fork = true;
        gh.with_repo(repo)
    }

    fn rollup_user() -> User {
        User::new(2000, "rolluper")
    }

    fn fork_repo() -> GithubRepoName {
        GithubRepoName::new(&rollup_user().name, default_repo_name().name())
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
