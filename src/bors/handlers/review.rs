use std::sync::Arc;

use crate::bors::command::Approver;
use crate::bors::command::RollupMode;
use crate::bors::event::PullRequestEdited;
use crate::bors::event::PullRequestPushed;
use crate::bors::handlers::deny_request;
use crate::bors::handlers::has_permission;
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::CommitSha;
use crate::github::GithubUser;
use crate::github::LabelTrigger;
use crate::github::PullRequest;
use crate::github::PullRequestNumber;
use crate::permissions::PermissionType;
use crate::PgDbClient;

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
    db.approve(
        repo_state.repository(),
        pr.number,
        approver.as_str(),
        priority,
        rollup,
    )
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
    db.unapprove(repo_state.repository(), pr.number).await?;
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
    db.set_priority(repo_state.repository(), pr.number, priority)
        .await
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

    let delegatee = pr.author.username.clone();
    db.delegate(repo_state.repository(), pr.number).await?;
    notify_of_delegation(&repo_state, pr, &delegatee).await
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
    db.undelegate(repo_state.repository(), pr.number).await
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
    db.set_rollup(repo_state.repository(), pr.number, rollup)
        .await
}

pub(super) async fn handle_pull_request_edited(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestEdited,
) -> anyhow::Result<()> {
    // If the base branch has changed, unapprove the PR
    let Some(_) = payload.from_base_sha else {
        return Ok(());
    };

    let pr_model = db
        .get_or_create_pull_request(repo_state.repository(), payload.pull_request.number)
        .await?;
    if !pr_model.is_approved() {
        return Ok(());
    }

    let pr_number = payload.pull_request.number;
    db.unapprove(repo_state.repository(), pr_number).await?;
    handle_label_trigger(&repo_state, pr_number, LabelTrigger::Unapproved).await?;
    notify_of_edited_pr(&repo_state, pr_number, &payload.pull_request.base.name).await
}

pub(super) async fn handle_push_to_pull_request(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestPushed,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_model = db
        .get_or_create_pull_request(repo_state.repository(), pr.number)
        .await?;

    if !pr_model.is_approved() {
        return Ok(());
    }

    let pr_number = pr_model.number;
    db.unapprove(repo_state.repository(), pr_number).await?;
    handle_label_trigger(&repo_state, pr_number, LabelTrigger::Unapproved).await?;
    notify_of_pushed_pr(&repo_state, pr_number, pr.head.sha.clone()).await
}

fn sufficient_delegate_permission(repo: Arc<RepositoryState>, author: &GithubUser) -> bool {
    repo.permissions
        .load()
        .has_permission(author.id, PermissionType::Review)
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

async fn notify_of_unapproval(repo: &RepositoryState, pr: &PullRequest) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!("Commit {} has been unapproved", pr.head.sha)),
        )
        .await
}

async fn notify_of_edited_pr(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    base_name: &str,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                r#":warning: The base branch changed to `{base_name}`, and the
PR will need to be re-approved."#,
            )),
        )
        .await
}

async fn notify_of_pushed_pr(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    head_sha: CommitSha,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                r#":warning: A new commit `{}` was pushed to the branch, the
PR will need to be re-approved."#,
                head_sha
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
    use crate::{
        bors::{
            handlers::{trybuild::TRY_MERGE_BRANCH_NAME, TRY_BRANCH_NAME},
            RollupMode,
        },
        github::PullRequestNumber,
        permissions::PermissionType,
        tests::mocks::{
            default_pr_number, default_repo_name, run_test, BorsBuilder, BorsTester, Comment,
            Permissions, PullRequestChangeEvent, User, World,
        },
    };

    #[sqlx::test]
    async fn default_approve(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{}`",
                        default_pr_number(),
                        User::default_user().name
                    ),
                );

                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
                        Ok(pr.rollup == Some(RollupMode::Maybe))
                    })
                    .await?;

                check_pr_approved_by(
                    &tester,
                    default_pr_number().into(),
                    &User::default_user().name,
                )
                .await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                let approve_user = "user1";
                tester
                    .post_comment(format!(r#"@bors r={approve_user}"#).as_str())
                    .await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{approve_user}`",
                        default_pr_number(),
                    ),
                );

                check_pr_approved_by(&tester, default_pr_number().into(), approve_user).await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_approve(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().permissions = Permissions::default();

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors try").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "@default-user: :key: Insufficient privileges: not in try users"
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_set_priority(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().permissions = Permissions::default();

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors p=2").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn unapprove(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{}`",
                        default_pr_number(),
                        User::default_user().name
                    ),
                );
                check_pr_approved_by(
                    &tester,
                    default_pr_number().into(),
                    &User::default_user().name,
                )
                .await;
                tester.post_comment("@bors r-").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!("Commit pr-{}-sha has been unapproved", default_pr_number()),
                );
                check_pr_unapproved(&tester, default_pr_number().into()).await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn unapprove_on_base_edited(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{}`",
                        default_pr_number(),
                        User::default_user().name
                    ),
                );
                tester
                    .edit_pull_request(
                        default_pr_number(),
                        PullRequestChangeEvent {
                            from_base_sha: Some("main-sha".to_string()),
                        },
                    )
                    .await?;

                assert_eq!(
                    tester.get_comment().await?,
                    r#":warning: The base branch changed to `main`, and the
PR will need to be re-approved."#,
                );
                check_pr_unapproved(&tester, default_pr_number().into()).await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_base_not_edited(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{}`",
                        default_pr_number(),
                        User::default_user().name
                    ),
                );
                tester
                    .edit_pull_request(
                        default_pr_number(),
                        PullRequestChangeEvent {
                            from_base_sha: None,
                        },
                    )
                    .await?;

                check_pr_approved_by(
                    &tester,
                    default_pr_number().into(),
                    &User::default_user().name,
                )
                .await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn edit_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .edit_pull_request(
                    default_pr_number(),
                    PullRequestChangeEvent {
                        from_base_sha: Some("main-sha".to_string()),
                    },
                )
                .await?;

            // No comment should be posted
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_on_push(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors r+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "Commit pr-{}-sha has been approved by `{}`",
                        default_pr_number(),
                        User::default_user().name
                    ),
                );
                tester.push_to_pull_request(default_pr_number()).await?;

                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        r#":warning: A new commit `pr-{}-sha` was pushed to the branch, the
PR will need to be re-approved."#,
                        default_pr_number()
                    )
                );
                check_pr_unapproved(&tester, default_pr_number().into()).await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn push_to_pr_do_nothing_when_not_approved(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.push_to_pull_request(default_pr_number()).await?;

            // No comment should be posted
            Ok(tester)
        })
        .await;
    }

    fn create_world_with_approve_config() -> World {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
approve = ["+approved"]
"#,
        );
        world
    }

    async fn check_pr_approved_by(
        tester: &BorsTester,
        pr_number: PullRequestNumber,
        approved_by: &str,
    ) {
        let pr_in_db = tester
            .db()
            .get_or_create_pull_request(&default_repo_name(), pr_number)
            .await
            .unwrap();
        assert_eq!(pr_in_db.approved_by, Some(approved_by.to_string()));
        let repo = tester.default_repo();
        let pr = repo.lock().get_pr(default_pr_number()).clone();
        pr.check_added_labels(&["approved"]);
    }

    async fn check_pr_unapproved(tester: &BorsTester, pr_number: PullRequestNumber) {
        let pr_in_db = tester
            .db()
            .get_or_create_pull_request(&default_repo_name(), pr_number)
            .await
            .unwrap();
        assert_eq!(pr_in_db.approved_by, None);
        let repo = tester.default_repo();
        let pr = repo.lock().get_pr(default_pr_number()).clone();
        pr.check_removed_labels(&["approved"]);
    }

    #[sqlx::test]
    async fn approve_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(10));
            assert!(pr.is_approved());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 p=10").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(10));
            assert_eq!(pr.approved_by, Some("user1".to_string()));
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
                    let pr = tester.get_default_pr().await?;
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
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.priority == Some(5))
                })
                .await?;

            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(5));
            assert!(pr.is_approved());

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
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.priority == Some(5))
                })
                .await?;

            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(10));
            assert!(pr.is_approved());

            Ok(tester)
        })
        .await;
    }

    fn reviewer() -> User {
        User::new(10, "reviewer")
    }

    fn as_reviewer(text: &str) -> Comment {
        Comment::from(text).with_author(reviewer())
    }

    fn create_world_with_delegate_config() -> World {
        let world = World::default();
        world.default_repo().lock().set_config(
            r#"
[labels]
approve = ["+approved"]
"#,
        );
        world.default_repo().lock().permissions = Permissions::default();
        world
            .default_repo()
            .lock()
            .permissions
            .users
            .insert(reviewer(), vec![PermissionType::Review]);

        world
    }

    #[sqlx::test]
    async fn delegate_author(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "@{} can now approve this pull request",
                        User::default().name
                    )
                );

                let pr = tester.get_default_pr().await?;
                assert!(pr.delegated);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_approve(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors r+").await?;
                tester.expect_comments(1).await;

                check_pr_approved_by(&tester, default_pr_number().into(), &User::default().name)
                    .await;
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_try(pool: sqlx::PgPool) {
        let world = BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors try").await?;
                tester.expect_comments(1).await;

                Ok(tester)
            })
            .await;
        world.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-main-sha1-pr-1-sha-0"],
        );
        world.check_sha_history(
            default_repo_name(),
            TRY_BRANCH_NAME,
            &["merge-main-sha1-pr-1-sha-0"],
        );
    }

    #[sqlx::test]
    async fn delegatee_can_set_priority(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors p=7").await?;
                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
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
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors delegate+").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "@{}: :key: Insufficient privileges: not in review users",
                        User::default().name
                    )
                );

                let pr = tester.get_default_pr().await?;
                assert!(!pr.delegated);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_by_reviewer(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                let pr = tester.get_default_pr().await?;
                assert!(pr.delegated);

                tester.post_comment(as_reviewer("@bors delegate-")).await?;
                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
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
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester.post_comment("@bors delegate-").await?;
                tester
                    .wait_for(|| async {
                        let pr = tester.get_default_pr().await?;
                        Ok(!pr.delegated)
                    })
                    .await?;

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_insufficient_permission(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors delegate-").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    format!(
                        "@{}: :key: Insufficient privileges: not in review users",
                        User::default().name
                    )
                    .as_str()
                );

                let pr = tester.get_default_pr().await?;
                assert!(!pr.delegated);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn reviewer_unapprove_delegated_approval(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester
                    .post_comment(Comment::from("@bors r+").with_author(User::default()))
                    .await?;
                tester.expect_comments(1).await;
                check_pr_approved_by(&tester, default_pr_number().into(), &User::default().name)
                    .await;

                tester.post_comment(as_reviewer("@bors r-")).await?;
                tester.expect_comments(1).await;

                let pr = tester.get_default_pr().await?;
                assert!(!pr.is_approved());

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn non_author_cannot_use_delegation(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_delegate_config())
            .run_test(|mut tester| async {
                tester.post_comment(as_reviewer("@bors delegate+")).await?;
                tester.expect_comments(1).await;

                tester
                    .post_comment(
                        Comment::from("@bors r+").with_author(User::new(999, "not-the-author")),
                    )
                    .await?;
                tester.expect_comments(1).await;

                let pr = tester.get_default_pr().await?;
                assert!(!pr.is_approved());

                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_insufficient_permission_try_user(pool: sqlx::PgPool) {
        let world = World::default();
        let try_user = User::new(200, "try-user");
        world.default_repo().lock().permissions = Permissions::default();
        world
            .default_repo()
            .lock()
            .permissions
            .users
            .insert(try_user.clone(), vec![PermissionType::Try]);

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester
                    .post_comment(Comment::from("@bors delegate+").with_author(try_user))
                    .await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "@try-user: :key: Insufficient privileges: not in review users"
                );

                let pr = tester.get_default_pr().await?;
                assert!(!pr.delegated);
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup=never").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Never));
            assert!(pr.is_approved());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert!(pr.is_approved());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare_maybe(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ rollup-").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Maybe));
            assert!(pr.is_approved());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_priority_rollup(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r+ p=10 rollup=never").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(10));
            assert_eq!(pr.rollup, Some(RollupMode::Never));
            assert!(pr.is_approved());
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 rollup").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert_eq!(pr.approved_by, Some("user1".to_string()));
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors r=user1 rollup=always").await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert_eq!(pr.approved_by, Some("user1".to_string()));
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
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert_eq!(pr.priority, Some(10));
            assert_eq!(pr.approved_by, Some("user1".to_string()));
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .post_comment("@bors r=user1 rollup- priority=10")
                .await?;
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Maybe));
            assert_eq!(pr.priority, Some(10));
            assert_eq!(pr.approved_by, Some("user1".to_string()));
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
                    let pr = tester.get_default_pr().await?;
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
                    let pr = tester.get_default_pr().await?;
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
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.rollup == Some(RollupMode::Always))
                })
                .await?;

            tester.post_comment("@bors r+").await?;
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert!(pr.is_approved());

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
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.rollup == Some(RollupMode::Never))
                })
                .await?;

            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.rollup, Some(RollupMode::Always));
            assert!(pr.is_approved());

            Ok(tester)
        })
        .await;
    }
}
