use std::sync::Arc;

use crate::PgDbClient;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::bors::command::Approver;
use crate::bors::command::RollupMode;
use crate::bors::handlers::deny_request;
use crate::bors::handlers::has_permission;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::database::ApprovalInfo;
use crate::database::TreeState;
use crate::github::GithubUser;
use crate::github::LabelTrigger;
use crate::github::PullRequest;
use crate::permissions::PermissionType;

/// Approve a pull request.
/// A pull request can only be approved by a user of sufficient authority.
pub(super) async fn command_approve(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    approver: &Approver,
    priority: Option<u32>,
    rollup: Option<RollupMode>,
) -> anyhow::Result<()> {
    tracing::info!("Approving PR {}", pr.number);
    if !has_permission(&repo_state, author, pr, &db, PermissionType::Review).await? {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    };
    let approver = match approver {
        Approver::Myself => author.username.clone(),
        Approver::Specified(approver) => approver.clone(),
    };
    let approval_info = ApprovalInfo {
        approver: approver.clone(),
        sha: pr.head.sha.to_string(),
    };
    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.approve(&pr_model, approval_info, priority, rollup)
        .await?;
    handle_label_trigger(&repo_state, pr.number, LabelTrigger::Approved).await?;
    notify_of_approval(&repo_state, pr, approver.as_str()).await
}

/// Unapprove a pull request.
/// Pull request's author can also unapprove the pull request.
pub(super) async fn command_unapprove(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    tracing::info!("Unapproving PR {}", pr.number);
    if !has_permission(&repo_state, author, pr, &db, PermissionType::Review).await? {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    };
    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.unapprove(&pr_model).await?;
    handle_label_trigger(&repo_state, pr.number, LabelTrigger::Unapproved).await?;
    notify_of_unapproval(&repo_state, pr).await
}

/// Set the priority of a pull request.
/// Priority can only be set by a user of sufficient authority.
pub(super) async fn command_set_priority(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    priority: u32,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, &db, PermissionType::Review).await? {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    };
    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.set_priority(&pr_model, priority).await
}

/// Delegate approval authority of a pull request to its author.
pub(super) async fn command_delegate(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    tracing::info!("Delegating PR {} approval", pr.number);
    if !sufficient_delegate_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }

    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.delegate(&pr_model).await?;
    notify_of_delegation(&repo_state, pr, &pr.author.username).await
}

/// Revoke any previously granted delegation.
pub(super) async fn command_undelegate(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    tracing::info!("Undelegating PR {} approval", pr.number);
    if !has_permission(&repo_state, author, pr, &db, PermissionType::Review).await? {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }
    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.undelegate(&pr_model).await
}

/// Set the rollup of a pull request.
/// rollup can only be set by a user of sufficient authority.
pub(super) async fn command_set_rollup(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    rollup: RollupMode,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, &db, PermissionType::Review).await? {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }
    let pr_model = db
        .get_or_create_pull_request(
            repo_state.repository(),
            pr.number,
            &pr.base.name,
            pr.mergeable_state.clone().into(),
        )
        .await?;

    db.set_rollup(&pr_model, rollup).await
}

pub(super) async fn command_close_tree(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    priority: u32,
    comment_url: &str,
) -> anyhow::Result<()> {
    if !sufficient_approve_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    };
    db.upsert_repository(
        repo_state.repository(),
        TreeState::Closed {
            priority,
            source: comment_url.to_string(),
        },
    )
    .await?;
    notify_of_tree_closed(&repo_state, pr, priority).await
}

pub(super) async fn command_open_tree(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    if !sufficient_delegate_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }

    db.upsert_repository(repo_state.repository(), TreeState::Open)
        .await?;
    notify_of_tree_open(&repo_state, pr).await
}

fn sufficient_approve_permission(repo: Arc<RepositoryState>, author: &GithubUser) -> bool {
    repo.permissions
        .load()
        .has_permission(author.id, PermissionType::Review)
}

fn sufficient_delegate_permission(repo: Arc<RepositoryState>, author: &GithubUser) -> bool {
    repo.permissions
        .load()
        .has_permission(author.id, PermissionType::Review)
}

async fn notify_of_tree_closed(
    repo: &RepositoryState,
    pr: &PullRequest,
    priority: u32,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!(
                "Tree closed for PRs with priority less than {}",
                priority
            )),
        )
        .await
}

async fn notify_of_tree_open(repo: &RepositoryState, pr: &PullRequest) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new("Tree is now open for merging".to_string()),
        )
        .await
}

async fn notify_of_unapproval(repo: &RepositoryState, pr: &PullRequest) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!("Commit {} has been unapproved", pr.head.sha)),
        )
        .await
}

async fn notify_of_approval(
    repo: &RepositoryState,
    pr: &PullRequest,
    approver: &str,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!(
                "Commit {} has been approved by `{}`",
                pr.head.sha, approver
            )),
        )
        .await
}

async fn notify_of_delegation(
    repo: &RepositoryState,
    pr: &PullRequest,
    delegatee: &str,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!("@{} can now approve this pull request", delegatee)),
        )
        .await
}

#[cfg(test)]
mod tests {
    use crate::database::TreeState;
    use crate::{
        bors::{
            RollupMode,
            handlers::{TRY_BRANCH_NAME, trybuild::TRY_MERGE_BRANCH_NAME},
        },
        tests::mocks::{
            BorsBuilder, Comment, GitHubState, Permissions, User, default_pr_number,
            default_repo_name, run_test,
        },
    };

    #[sqlx::test]
    async fn default_approve(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"Commit pr-1-sha has been approved by `default-user`"
            );

            tester
                .default_pr()
                .await
                .expect_rollup(None)
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            let approve_user = "user1";
            tester
                .post_comment(format!(r#"@bors r={approve_user}"#).as_str())
                .await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"Commit pr-1-sha has been approved by `user1`"
            );

            tester.default_pr().await.expect_approved_by(approve_user);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_approve(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment(Comment::from("@bors try").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in try users"
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_set_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment(Comment::from("@bors p=2").with_author(User::unprivileged()))
                .await?;
            tester.post_comment("@bors p=2").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in review users"
            );
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"Commit pr-1-sha has been approved by `default-user`",
            );
            tester
                .default_pr()
                .await
                .expect_approved_by(&User::default_pr_author().name);
            tester.post_comment("@bors r-").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"Commit pr-1-sha has been unapproved"
            );

            tester.default_pr().await.expect_unapproved();
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_priority(Some(10))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 p=10").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_priority(Some(10))
                .expect_approved_by("user1");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn set_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors p=5").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.priority == Some(5))
                })
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn priority_preserved_after_approve(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors p=5").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.priority == Some(5))
                })
                .await?;

            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester.default_pr().await.expect_priority(Some(5));

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn priority_overridden_on_approve_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors p=5").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.priority == Some(5))
                })
                .await?;

            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments(1).await;

            tester.default_pr().await.expect_priority(Some(10));

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn tree_closed_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors treeclosed=5").await?;
            insta::assert_snapshot!(
                tester.get_comment().await?,
                @"Tree closed for PRs with priority less than 5"
            );

            let repo = tester.db().repo_db(&default_repo_name()).await?;
            assert_eq!(
                repo.unwrap().tree_state,
                TreeState::Closed {
                    priority: 5,
                    source: format!(
                        "https://github.com/{}/pull/1#issuecomment-1",
                        default_repo_name()
                    ),
                }
            );

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_tree_closed(pool: sqlx::PgPool) {
        let gh = GitHubState::default();
        gh.default_repo().lock().permissions = Permissions::empty();

        BorsBuilder::new(pool)
            .github(gh)
            .run_test(|mut tester| async {
                tester.post_comment("@bors treeclosed=5").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(tester)
            })
            .await;
    }

    fn review_comment(text: &str) -> Comment {
        Comment::from(text).with_author(User::reviewer())
    }

    #[sqlx::test]
    async fn cannot_approve_without_delegation(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_author(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @"@default-user can now approve this pull request"
                );

                tester.default_pr().await.expect_delegated();
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_approve(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;

                tester
                    .default_pr()
                    .await
                    .expect_approved_by(&User::default_pr_author().name);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_try(pool: sqlx::PgPool) {
        let gh = BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;
                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                Ok(tester)
            })
            .await;
        gh.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-main-sha1-pr-1-sha-0"],
        );
        gh.check_sha_history(
            default_repo_name(),
            TRY_BRANCH_NAME,
            &["merge-main-sha1-pr-1-sha-0"],
        );
    }

    #[sqlx::test]
    async fn delegatee_can_set_priority(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors p=7").await?;
                tester
                    .wait_for(|| async {
                        let Some(pr) = tester.default_pr_db().await? else {
                            return Ok(false);
                        };
                        Ok(pr.priority == Some(7))
                    })
                    .await?;

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_insufficient_permission(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester.post_comment("@bors delegate+").await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );

                assert!(tester.default_pr_db().await?.is_none());
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_by_reviewer(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;
                tester.default_pr().await.expect_delegated();

                tester
                    .post_comment(review_comment("@bors delegate-"))
                    .await?;
                tester
                    .wait_for(|| async {
                        let Some(pr) = tester.default_pr_db().await? else {
                            return Ok(false);
                        };
                        Ok(!pr.delegated)
                    })
                    .await?;

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_by_delegatee(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors delegate-").await?;
                tester
                    .wait_for(|| async {
                        let Some(pr) = tester.default_pr_db().await? else {
                            return Ok(false);
                        };
                        Ok(!pr.delegated)
                    })
                    .await?;

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn reviewer_unapprove_delegated_approval(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;
                tester
                    .default_pr()
                    .await
                    .expect_approved_by(&User::default_pr_author().name);

                tester.post_comment(review_comment("@bors r-")).await?;
                tester.expect_comments(1).await;

                tester.default_pr().await.expect_unapproved();

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn non_author_cannot_use_delegation(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHubState::unauthorized_pr_author())
            .run_test(|mut tester| async {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments(1).await;

                tester
                    .post_comment(Comment::from("@bors r+").with_author(User::unprivileged()))
                    .await?;
                tester.expect_comments(1).await;

                tester.default_pr().await.expect_unapproved();

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_insufficient_permission_try_user(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .run_test(|mut tester| async {
                tester
                    .post_comment(Comment::from("@bors delegate+").with_author(User::try_user()))
                    .await?;
                insta::assert_snapshot!(
                    tester.get_comment().await?,
                    @"@user-with-try-privileges: :key: Insufficient privileges: not in review users"
                );
                assert!(tester.default_pr_db().await?.is_none());
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup=never").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Never))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare_maybe(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup-").await?;
            tester.expect_comments(1).await;
            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Maybe))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_priority_rollup(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ p=10 rollup=never").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_priority(Some(10))
                .expect_rollup(Some(RollupMode::Never))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 rollup").await?;
            tester.expect_comments(1).await;
            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 rollup=always").await?;
            tester.expect_comments(1).await;
            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment("@bors r=user1 rollup=always priority=10")
                .await?;
            tester.expect_comments(1).await;
            tester
                .default_pr()
                .await
                .expect_priority(Some(10))
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn set_rollup_default(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors rollup").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.rollup == Some(RollupMode::Always))
                })
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn set_rollup_with_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors rollup=maybe").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.rollup == Some(RollupMode::Maybe))
                })
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_preserved_after_approve(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors rollup").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.rollup == Some(RollupMode::Always))
                })
                .await?;

            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_overridden_on_approve_with_rollup(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors rollup=never").await?;
            tester
                .wait_for(|| async {
                    let Some(pr) = tester.default_pr_db().await? else {
                        return Ok(false);
                    };
                    Ok(pr.rollup == Some(RollupMode::Never))
                })
                .await?;

            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments(1).await;

            tester
                .default_pr()
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_store_sha(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            let pr = tester.default_pr().await.get_gh_pr();
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester.default_pr().await.expect_approved_sha(&pr.head_sha);

            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn reapproved_pr_uses_latest_sha(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            let pr = tester.default_pr().await.get_gh_pr();
            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester.default_pr().await.expect_approved_sha(&pr.head_sha);

            tester
                .push_to_pr(default_repo_name(), default_pr_number())
                .await?;
            let pr2 = tester.default_pr().await.get_gh_pr();
            assert_ne!(pr.head_sha, pr2.head_sha);

            tester.expect_comments(1).await;

            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            tester.default_pr().await.expect_approved_sha(&pr2.head_sha);

            Ok(tester)
        })
        .await;
    }
}
