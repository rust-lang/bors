use std::sync::Arc;

use crate::bors::command::Approver;
use crate::bors::event::PullRequestEdited;
use crate::bors::event::PullRequestPushed;
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
) -> anyhow::Result<()> {
    tracing::info!("Approving PR {}", pr.number);
    if !sufficient_approve_permission(repo_state.clone(), author) {
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
    if !sufficient_unapprove_permission(repo_state.clone(), pr, author) {
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
    if !sufficient_priority_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    };
    db.set_priority(repo_state.repository(), pr.number, priority)
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

pub(super) async fn command_tree_closed(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
    priority: u32,
) -> anyhow::Result<()> {
    if !sufficient_priority_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }
    
    db.update_repository_treeclosed(repo_state.repository(), priority).await?;
    notify_of_tree_closed(&repo_state, pr, priority).await
}

pub(super) async fn command_tree_open(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    if !sufficient_priority_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr, author, PermissionType::Review).await?;
        return Ok(());
    }
    
    db.update_repository_treeclosed(repo_state.repository(), 0).await?;
    notify_of_tree_open(&repo_state, pr).await
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

async fn notify_of_tree_open(
    repo: &RepositoryState,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.number,
            Comment::new("Tree is now open for merging".to_string()),
        )
        .await
}

fn sufficient_approve_permission(repo: Arc<RepositoryState>, author: &GithubUser) -> bool {
    repo.permissions
        .load()
        .has_permission(author.id, PermissionType::Review)
}

fn sufficient_priority_permission(repo: Arc<RepositoryState>, author: &GithubUser) -> bool {
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

fn sufficient_unapprove_permission(
    repo: Arc<RepositoryState>,
    pr: &PullRequest,
    author: &GithubUser,
) -> bool {
    author.id == pr.author.id
        || repo
            .permissions
            .load()
            .has_permission(author.id, PermissionType::Review)
}

async fn deny_request(
    repo: &RepositoryState,
    pr: &PullRequest,
    author: &GithubUser,
    permission_type: PermissionType,
) -> anyhow::Result<()> {
    tracing::warn!(
        "Permission denied for request command by {}",
        author.username
    );
    repo.client
        .post_comment(
            pr.number,
            Comment::new(format!(
                "@{}: :key: Insufficient privileges: not in {} users",
                author.username, permission_type
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

#[cfg(test)]
mod tests {
    use crate::{
        github::PullRequestNumber,
        database::TreeState,
        tests::mocks::{
            default_pr_number, default_repo_name, run_test, BorsBuilder, BorsTester, Permissions,
            PullRequestChangeEvent, User, World,
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
            // Wait for db update.
            tester.expect_comments(1).await;
            let pr = tester.get_default_pr().await?;

            assert_eq!(pr.priority, Some(5));
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn priority_preserved_after_approve(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors p=5").await?;
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(5));

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
            tester.expect_comments(1).await;

            let pr = tester.get_default_pr().await?;
            assert_eq!(pr.priority, Some(5));

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
    async fn tree_closed_with_priority(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .world(create_world_with_approve_config())
            .run_test(|mut tester| async {
                tester.post_comment("@bors treeclosed=5").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "Tree closed for PRs with priority less than 5"
                );
                
                // Verify the treeclosed state in the database
                let repo_models = tester.db()
                    .get_repository_treeclosed(&default_repo_name())
                    .await?;
                let repo_model = repo_models.first().unwrap();
                assert_eq!(repo_model.treeclosed, TreeState::Closed(5));
                
                Ok(tester)
            })
            .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_tree_closed(pool: sqlx::PgPool) {
        let world = World::default();
        world.default_repo().lock().permissions = Permissions::default();

        BorsBuilder::new(pool)
            .world(world)
            .run_test(|mut tester| async {
                tester.post_comment("@bors treeclosed=5").await?;
                assert_eq!(
                    tester.get_comment().await?,
                    "@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(tester)
            })
            .await;
    }
}
