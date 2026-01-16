use super::{GithubRepoName, PullRequest, PullRequestNumber};
use crate::PgDbClient;
use crate::bors::{make_text_ignored_by_bors, normalize_merge_message};
use crate::github::api::client::GithubRepositoryClient;
use crate::github::api::operations::MergeError;
use crate::github::oauth::{OAuthClient, UserGitHubClient};
use crate::permissions::PermissionType;
use crate::server::ServerStateRef;
use anyhow::Context;
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect, Response};
use futures::StreamExt;
use itertools::Itertools;
use rand::{Rng, distr::Alphanumeric};
use std::collections::HashSet;
use std::fmt::Write;
use std::sync::Arc;
use tracing::Instrument;

/// Maximum number of PRs that can be rolled up at once.
const ROLLUP_PR_LIMIT: u64 = 50;

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
    TooManyPullRequests,
    NotAuthenticated,
}

impl IntoResponse for RollupError {
    fn into_response(self) -> Response {
        match self {
            RollupError::Generic(error) => {
                (StatusCode::INTERNAL_SERVER_ERROR, format!("{error:?}"))
            }
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
            RollupError::TooManyPullRequests => (
                StatusCode::BAD_REQUEST,
                format!("Rolling up too many pull requests, at most {ROLLUP_PR_LIMIT} is allowed"),
            ),
            RollupError::NotAuthenticated => (
                StatusCode::FORBIDDEN,
                "You are not allowed to create rollups. Review permissions are required."
                    .to_string(),
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
        return Err(RollupError::BaseRepoNotFound { repo_name });
    };

    let user_client = oauth_client
        .get_authenticated_client(repo_name.clone(), &callback.code)
        .await?;

    // The rollup author is expected to r+ the rollup, so they must have review permissions to
    // create it in the first place.
    if !repo_state
        .permissions
        .load()
        .has_permission(user_client.user.id, PermissionType::Review)
    {
        return Err(RollupError::NotAuthenticated);
    }

    let span = tracing::info_span!(
        "create_rollup",
        repo = %repo_name,
        pr_nums = ?oauth_state.pr_nums
    );

    let pr = match create_rollup(
        db,
        oauth_state,
        state.get_web_url(),
        &repo_state.client,
        user_client,
    )
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
    Ok(Redirect::to(&pr_url).into_response())
}

/// Creates a rollup PR by merging multiple approved PRs into a single branch
/// in the user's fork, then opens a PR to the upstream repository.
async fn create_rollup(
    db: Arc<PgDbClient>,
    rollup_state: OAuthRollupState,
    web_url: &str,
    gh_client: &GithubRepositoryClient,
    user_client: UserGitHubClient,
) -> Result<PullRequest, RollupError> {
    let OAuthRollupState {
        repo_name,
        repo_owner,
        mut pr_nums,
    } = rollup_state;

    // Repository where we will create the PR
    let base_repo = GithubRepoName::new(&repo_owner, &repo_name);

    let username = user_client.user.username;

    tracing::info!("User {username} is creating a rollup with PRs: {pr_nums:?}");

    // Filter out duplicates
    let mut pr_set = HashSet::new();
    pr_nums.retain(|&pr| pr_set.insert(pr));

    if pr_nums.len() as u64 > ROLLUP_PR_LIMIT {
        return Err(RollupError::TooManyPullRequests);
    }
    if pr_nums.is_empty() {
        return Err(RollupError::NoPullRequestsSelected);
    }

    // Ensure user has a fork
    if user_client.client.get_repo().await.is_err() {
        return Err(RollupError::ForkNotFound {
            repo_name: user_client.client.repository().clone(),
        });
    };

    // Validate PRs
    let mut rollup_prs = Vec::new();
    for &num in &pr_nums {
        match db.get_pull_request(&base_repo, (num as u64).into()).await? {
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
    let base_branch = first_pr_github.base.name.clone();

    // Fetch the current SHA of the base branch - this is the commit our
    // rollup branch starts from.
    let base_branch_sha = gh_client.get_branch_sha(&base_branch).await?;

    let branch_suffix: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(7)
        .map(char::from)
        .collect();
    let rollup_branch = format!("rollup-{branch_suffix}");

    tracing::debug!(
        "Setting rollup branch {rollup_branch} to SHA {base_branch_sha} ({base_branch})"
    );

    // Create the branch on the user's fork
    user_client
        .client
        .create_branch(&rollup_branch, &base_branch_sha)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not create rollup branch {rollup_branch}: {error:?}")
        })?;

    let mut successes = Vec::new();
    let mut failures = Vec::new();

    let mut github_prs = Vec::with_capacity(pr_nums.len());
    github_prs.push(first_pr_github);

    // Download the rest of the PRs concurrently, with at most 10 concurrent requests in-flight
    {
        // The collect here is because of implementation of `FnOnce` is not general enough :(
        let pr_nums = rollup_prs
            .iter()
            .skip(1)
            .map(|pr| pr.number)
            .collect::<Vec<_>>();
        let mut pr_stream = futures::stream::iter(
            pr_nums
                .into_iter()
                .map(|pr_num| gh_client.get_pull_request(pr_num)),
        )
        .buffered(10);
        while let Some(pr) = pr_stream.next().await {
            github_prs.push(pr?);
        }
    }

    assert_eq!(rollup_prs.len(), github_prs.len());

    // Merge each PR's commits into the rollup branch
    for (pr, pr_github) in rollup_prs.into_iter().zip(github_prs) {
        assert_eq!(pr.number, pr_github.number);

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
        let merge_msg = normalize_merge_message(&merge_msg);

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
        body.push_str(&format!(
            " - {}#{} ({})\n",
            gh_client.repository(),
            pr.number.0,
            pr.title
        ));
    }

    if !failures.is_empty() {
        body.push_str("\nFailed merges:\n\n");
        for pr in &failures {
            body.push_str(&format!(
                " - {}#{} ({})\n",
                gh_client.repository(),
                pr.number.0,
                pr.title
            ));
        }
    }
    body.push_str("\nr? @ghost");

    let similar_rollup_link = format!(
        "[Create a similar rollup]({web_url}/queue/{repo_name}?prs={})",
        pr_nums.iter().copied().map(|s| s.to_string()).join(",")
    );
    writeln!(
        body,
        "\n\n{}\n",
        make_text_ignored_by_bors(&similar_rollup_link)
    )?;

    let title = format!("Rollup of {} pull requests", successes.len());

    // Create the rollup PR from the user's fork branch to the main repo base branch
    let pr = user_client
        .client
        .create_pr(
            &base_repo,
            &title,
            &format!("{username}:{rollup_branch}"),
            &base_branch,
            &body,
        )
        .await
        .context("Cannot create PR")?;

    // Set the rollup label
    gh_client
        .add_labels(pr.number, &["rollup".to_string()])
        .await?;

    Ok(pr)
}

#[cfg(test)]
mod tests {
    use crate::github::rollup::OAuthRollupState;
    use crate::github::{GithubRepoName, PullRequestNumber};
    use crate::permissions::PermissionType;
    use crate::tests::{
        ApiRequest, ApiResponse, BorsTester, Comment, GitHub, MergeBehavior, PullRequest, Repo,
        User, default_repo_name, run_test,
    };
    use http::StatusCode;

    #[sqlx::test]
    async fn rollup_missing_fork(pool: sqlx::PgPool) {
        let mut gh = GitHub::default();
        gh.add_user(rollup_user());
        gh.get_repo(())
            .lock()
            .permissions
            .users
            .insert(rollup_user(), vec![PermissionType::Review]);

        run_test((pool, gh), async |ctx: &mut BorsTester| {
            let pr1 = ctx.open_pr((), |_| {}).await?;
            let pr2 = ctx.open_pr((), |_| {}).await?;

            make_rollup(ctx, &[&pr1, &pr2])
                .await?
                .assert_status(StatusCode::BAD_REQUEST)
                .assert_body(&format!(
                    "Fork rolluper/{} not found, create it before opening the rollup",
                    pr1.repo().name()
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
                    pr1.number()
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
    async fn rollup_too_many_prs(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let prs = (1..60).map(PullRequestNumber).collect::<Vec<_>>();
            ctx.api_request(rollup_request(
                &rollup_user().name,
                default_repo_name(),
                &prs,
            ))
            .await?
            .assert_status(StatusCode::BAD_REQUEST)
            .assert_body("Rolling up too many pull requests, at most 50 is allowed");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_missing_review_permissions(pool: sqlx::PgPool) {
        let mut gh = GitHub::default();
        gh.add_user(rollup_user());
        run_test((pool, gh), async |ctx: &mut BorsTester| {
            let pr1 = ctx.open_pr((), |_| {}).await?;

            make_rollup(ctx, &[&pr1])
                .await?
                .assert_status(StatusCode::FORBIDDEN)
                .assert_body(
                    "You are not allowed to create rollups. Review permissions are required.",
                );
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
                    // Cause a conflict on the third merge
                    if n == 3 {
                        Some(StatusCode::CONFLICT)
                    } else {
                        None
                    }
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
                .assert_status(StatusCode::SEE_OTHER);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(6).description, @"
        Successful merges:

         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#3 (Title of PR 3)
         - rust-lang/borstest#5 (Title of PR 5)

        Failed merges:

         - rust-lang/borstest#4 (Title of PR 4)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=2,3,4,5)
        <!-- homu-ignore:end -->
        ");
    }

    #[sqlx::test]
    async fn rollup_different_base_branch(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let beta = ctx.create_branch("beta");
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx
                .open_pr((), |pr| {
                    pr.base_branch = beta;
                })
                .await?;
            let pr4 = ctx.open_pr((), |_| {}).await?;
            for pr in &[&pr2, &pr3, &pr4] {
                ctx.approve(pr.id()).await?;
            }

            make_rollup(ctx, &[&pr2, &pr3, &pr4])
                .await?
                .assert_status(StatusCode::SEE_OTHER);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(5).description, @"
        Successful merges:

         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#4 (Title of PR 4)

        Failed merges:

         - rust-lang/borstest#3 (Title of PR 3)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=2,3,4)
        <!-- homu-ignore:end -->
        ");
    }

    #[sqlx::test]
    async fn rollup_recover_merge_error(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let mut fail_counter = 2u32;
            ctx.modify_repo(fork_repo(), |repo| {
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    fail_counter = fail_counter.saturating_sub(1);
                    if fail_counter == 0 {
                        None
                    } else {
                        Some(StatusCode::INTERNAL_SERVER_ERROR)
                    }
                }))
            });

            let pr_url = make_rollup(ctx, &[&pr2, &pr3])
                .await?
                .assert_status(StatusCode::SEE_OTHER)
                .get_header("location");
            insta::assert_snapshot!(pr_url, @"https://github.com/rust-lang/borstest/pull/4");

            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(4).description, @"
        Successful merges:

         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#3 (Title of PR 3)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=2,3)
        <!-- homu-ignore:end -->
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
                .assert_status(StatusCode::SEE_OTHER)
                .get_header("location");
            insta::assert_snapshot!(pr_url, @"https://github.com/rust-lang/borstest/pull/4");

            ctx.pr(4).await.expect_added_labels(&["rollup"]);

            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(4).description, @"
        Successful merges:

         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#3 (Title of PR 3)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=2,3)
        <!-- homu-ignore:end -->
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
                .assert_status(StatusCode::SEE_OTHER);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(6).description, @"
        Successful merges:

         - rust-lang/borstest#3 (Title of PR 3)
         - rust-lang/borstest#4 (Title of PR 4)
         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#5 (Title of PR 5)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=2,3,4,5)
        <!-- homu-ignore:end -->
        ");
    }

    #[sqlx::test]
    async fn rollup_duplicates(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            let pr3 = ctx.open_pr(default_repo_name(), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            make_rollup(ctx, &[&pr3, &pr2, &pr3, &pr2])
                .await?
                .assert_status(StatusCode::SEE_OTHER);
            Ok(())
        })
        .await;
        let repo = gh.get_repo(());
        insta::assert_snapshot!(repo.lock().get_pr(4).description, @"
        Successful merges:

         - rust-lang/borstest#2 (Title of PR 2)
         - rust-lang/borstest#3 (Title of PR 3)

        r? @ghost

        <!-- homu-ignore:start -->
        [Create a similar rollup](https://bors-test.com/queue/borstest?prs=3,2)
        <!-- homu-ignore:end -->
        ");
    }

    #[sqlx::test]
    async fn rollup_remove_homu_ignore_block(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx
                .open_pr(default_repo_name(), |pr| {
                    pr.description = r"This is a very good PR.

<!-- homu-ignore:start -->
ignore this 1
<!-- homu-ignore:end -->

include this

<!-- homu-ignore:start -->
ignore this 2
<!-- homu-ignore:end -->

also include this pls"
                        .to_string();
                })
                .await?;
            ctx.approve(pr2.id()).await?;

            make_rollup(ctx, &[&pr2])
                .await?
                .assert_status(StatusCode::SEE_OTHER);
            Ok(())
        })
        .await;
        let rollup_branch = gh
            .get_repo(fork_repo())
            .lock()
            .branches()
            .iter()
            .find(|branch| branch.name().starts_with("rollup"))
            .unwrap()
            .clone();
        // Find the rollup merge commit
        let rollup_merge_commit = rollup_branch.get_commit_history().last().unwrap().clone();
        insta::assert_snapshot!(rollup_merge_commit.message(), @"
        Rollup merge of #2 - pr/2, r=default-user

        Title of PR 2

        This is a very good PR.



        include this



        also include this pls
        ");
    }

    async fn make_rollup(
        ctx: &mut BorsTester,
        prs: &[&PullRequest],
    ) -> anyhow::Result<ApiResponse> {
        let prs = prs.iter().map(|pr| pr.number()).collect::<Vec<_>>();
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
        gh.get_repo(())
            .lock()
            .permissions
            .users
            .insert(rolluper.clone(), vec![PermissionType::Review]);

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
