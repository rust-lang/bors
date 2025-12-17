use std::sync::Arc;

use crate::bors::RepositoryState;
use crate::bors::command::RollupMode;
use crate::bors::command::{Approver, CommandPrefix};
use crate::bors::comment::{
    approve_blocking_labels_present, approve_non_open_pr_comment, approve_wip_title,
    approved_comment, delegate_comment, delegate_try_builds_comment, unapprove_non_open_pr_comment,
};
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::handlers::workflow::{AutoBuildCancelReason, maybe_cancel_auto_build};
use crate::bors::handlers::{PullRequestData, deny_request};
use crate::bors::handlers::{has_permission, unapprove_pr};
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{Comment, PullRequestStatus};
use crate::database::ApprovalInfo;
use crate::database::DelegatedPermission;
use crate::database::TreeState;
use crate::github::LabelTrigger;
use crate::github::{GithubUser, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::{BorsContext, PgDbClient};

/// Approve a pull request.
/// A pull request can only be approved by a user of sufficient authority.
#[allow(clippy::too_many_arguments)]
pub(super) async fn command_approve(
    ctx: Arc<BorsContext>,
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    approver: &Approver,
    priority: Option<u32>,
    rollup: Option<RollupMode>,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    tracing::info!("Approving PR {}", pr.number());
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    };

    if let Some(error_comment) = check_pr_approval_validity(pr, &repo_state).await? {
        repo_state
            .client
            .post_comment(pr.number(), error_comment)
            .await?;
        return Ok(());
    }

    let approver = match approver {
        Approver::Myself => author.username.clone(),
        Approver::Specified(approver) => approver.clone(),
    };
    let approval_info = ApprovalInfo {
        approver: approver.clone(),
        sha: pr.github.head.sha.to_string(),
    };

    db.approve(pr.db, approval_info, priority, rollup).await?;
    handle_label_trigger(&repo_state, pr.number(), LabelTrigger::Approved).await?;

    merge_queue_tx.notify().await?;
    notify_of_approval(ctx, &repo_state, pr, approver.as_str()).await
}

/// Keywords that will prevent an approval if they appear in the PR's title.
/// They are checked in a case-insensitive manner.
const WIP_KEYWORDS: &[&str] = &["wip", "[do not merge]"];

/// Check that the given PR can be approved in its current state.
/// Returns `Ok(Some(comment))` if it **cannot** be approved; the comment should be sent to the
/// pull request.
async fn check_pr_approval_validity(
    pr: PullRequestData<'_>,
    repo: &RepositoryState,
) -> anyhow::Result<Option<Comment>> {
    // Check PR status
    if !matches!(pr.github.status, PullRequestStatus::Open) {
        return Ok(Some(approve_non_open_pr_comment()));
    }

    // Check WIP title
    let title = pr.github.title.to_lowercase();
    if let Some(wip_kw) = WIP_KEYWORDS.iter().find(|kw| title.contains(*kw)) {
        return Ok(Some(approve_wip_title(wip_kw)));
    }

    // Check blocking labels
    let config = repo.config.load();
    let blocking_labels: Vec<&str> = pr
        .github
        .labels
        .iter()
        .map(|label| label.as_str())
        .filter(|label| {
            config
                .labels_blocking_approval
                .iter()
                .any(|blocking_label| blocking_label == label)
        })
        .collect();
    if !blocking_labels.is_empty() {
        return Ok(Some(approve_blocking_labels_present(&blocking_labels)));
    }

    Ok(None)
}

/// Unapprove a pull request.
/// Pull request's author can also unapprove the pull request.
pub(super) async fn command_unapprove(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
) -> anyhow::Result<()> {
    let pr_num = pr.number();

    tracing::info!("Unapproving PR {}", pr_num);
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr_num, author, PermissionType::Review).await?;
        return Ok(());
    };

    if !matches!(
        pr.github.status,
        PullRequestStatus::Open | PullRequestStatus::Draft
    ) {
        repo_state
            .client
            .post_comment(pr_num, unapprove_non_open_pr_comment())
            .await?;
        return Ok(());
    }

    let auto_build_cancel_message = maybe_cancel_auto_build(
        &repo_state.client,
        &db,
        pr.db,
        AutoBuildCancelReason::Unapproval,
    )
    .await?;
    unapprove_pr(&repo_state, &db, pr.db).await?;
    notify_of_unapproval(&repo_state, pr, auto_build_cancel_message).await?;

    Ok(())
}

/// Set the priority of a pull request.
/// Priority can only be set by a user of sufficient authority.
pub(super) async fn command_set_priority(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    priority: u32,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    };
    db.set_priority(pr.db, priority).await
}

/// Delegate permissions of a pull request to its author.
pub(super) async fn command_delegate(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    delegated_permission: DelegatedPermission,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    tracing::info!(
        "Delegating PR {} {} permissions",
        pr.number(),
        delegated_permission
    );
    if !sufficient_delegate_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }

    db.delegate(pr.db, delegated_permission).await?;
    notify_of_delegation(
        &repo_state,
        pr.number(),
        &pr.github.author.username,
        &author.username,
        delegated_permission,
        bot_prefix,
    )
    .await
}

/// Revoke any previously granted delegation.
pub(super) async fn command_undelegate(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
) -> anyhow::Result<()> {
    tracing::info!("Undelegating PR {} approval", pr.number());
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }
    db.undelegate(pr.db).await
}

/// Set the rollup of a pull request.
/// rollup can only be set by a user of sufficient authority.
pub(super) async fn command_set_rollup(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    rollup: RollupMode,
) -> anyhow::Result<()> {
    if !has_permission(&repo_state, author, pr, PermissionType::Review).await? {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }
    db.set_rollup(pr.db, rollup).await
}

pub(super) async fn command_close_tree(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    priority: u32,
    comment_url: &str,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !sufficient_approve_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
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

    merge_queue_tx.notify().await?;
    notify_of_tree_closed(&repo_state, pr.number(), priority).await
}

pub(super) async fn command_open_tree(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    pr: PullRequestData<'_>,
    author: &GithubUser,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    if !sufficient_delegate_permission(repo_state.clone(), author) {
        deny_request(&repo_state, pr.number(), author, PermissionType::Review).await?;
        return Ok(());
    }

    db.upsert_repository(repo_state.repository(), TreeState::Open)
        .await?;

    merge_queue_tx.notify().await?;
    notify_of_tree_open(&repo_state, pr.number()).await
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
    pr_number: PullRequestNumber,
    priority: u32,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new(format!(
                "Tree closed for PRs with priority less than {priority}"
            )),
        )
        .await?;
    Ok(())
}

async fn notify_of_tree_open(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr_number,
            Comment::new("Tree is now open for merging".to_string()),
        )
        .await?;
    Ok(())
}

async fn notify_of_unapproval(
    repo: &RepositoryState,
    pr: PullRequestData<'_>,
    cancel_message: Option<String>,
) -> anyhow::Result<()> {
    let mut comment = format!("Commit {} has been unapproved.", pr.github.head.sha);

    if let Some(message) = cancel_message {
        comment.push_str(&format!("\n\n{message}"));
    }

    repo.client
        .post_comment(pr.number(), Comment::new(comment))
        .await?;
    Ok(())
}

async fn notify_of_approval(
    ctx: Arc<BorsContext>,
    repo: &RepositoryState,
    pr: PullRequestData<'_>,
    approver: &str,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(
            pr.db.number,
            approved_comment(
                ctx.get_web_url(),
                repo.repository(),
                &pr.github.head.sha,
                approver,
            ),
        )
        .await?;
    Ok(())
}

async fn notify_of_delegation(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    delegatee: &str,
    delegator: &str,
    delegated_permission: DelegatedPermission,
    bot_prefix: &CommandPrefix,
) -> anyhow::Result<()> {
    let comment = match delegated_permission {
        DelegatedPermission::Try => delegate_try_builds_comment(delegatee, bot_prefix),
        DelegatedPermission::Review => delegate_comment(delegatee, delegator, bot_prefix),
    };

    repo.client.post_comment(pr_number, comment).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};

    use crate::bors::TRY_BRANCH_NAME;
    use crate::bors::merge_queue::AUTO_BUILD_CHECK_RUN_NAME;
    use crate::database::{DelegatedPermission, TreeState};
    use crate::tests::BorsTester;
    use crate::tests::default_repo_name;
    use crate::{
        bors::{RollupMode, handlers::trybuild::TRY_MERGE_BRANCH_NAME},
        tests::{BorsBuilder, Comment, GitHub, Permissions, User, run_test},
    };

    #[sqlx::test]
    async fn default_approve(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :pushpin: Commit pr-1-sha has been approved by `default-user`

            It is now in the [queue](https://test.com/bors/queue/borstest) for this repository.
            "
            );

            tester
                .get_pr_copy(())
                .await
                .expect_rollup(None)
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let approve_user = "user1";
            tester
                .post_comment(format!(r#"@bors r={approve_user}"#).as_str())
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :pushpin: Commit pr-1-sha has been approved by `user1`

            It is now in the [queue](https://test.com/bors/queue/borstest) for this repository.
            "
            );

            tester
                .get_pr_copy(())
                .await
                .expect_approved_by(approve_user);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_approve(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(Comment::from("@bors try").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in try users"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_set_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(Comment::from("@bors p=2").with_author(User::unprivileged()))
                .await?;
            tester.post_comment("@bors p=2").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in review users"
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @r"
            :pushpin: Commit pr-1-sha has been approved by `default-user`

            It is now in the [queue](https://test.com/bors/queue/borstest) for this repository.
            ",
            );
            tester
                .get_pr_copy(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            tester.post_comment("@bors r-").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"Commit pr-1-sha has been unapproved."
            );

            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_lacking_permissions(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester
                .post_comment(Comment::from("@bors r-").with_author(User::unprivileged()))
                .await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @"@unprivileged-user: :key: Insufficient privileges: not in review users"
            );

            tester
                .get_pr_copy(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_merged_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.set_pr_status_closed(()).await?;
            tester.post_comment("@bors r-").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
                @":clipboard: Only unclosed PRs can be unapproved."
            );

            tester
                .get_pr_copy(())
                .await
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_priority(Some(10))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r=user1 p=10").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_priority(Some(10))
                .expect_approved_by("user1");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn set_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors p=5").await?;
            tester.wait_for_pr((), |pr| pr.priority == Some(5)).await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn priority_preserved_after_approve(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors p=5").await?;
            tester.wait_for_pr((), |pr| pr.priority == Some(5)).await?;

            tester.approve(()).await?;

            tester.get_pr_copy(()).await.expect_priority(Some(5));

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn priority_overridden_on_approve_with_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors p=5").await?;
            tester.wait_for_pr((), |pr| pr.priority == Some(5)).await?;

            tester.post_comment("@bors r+ p=10").await?;
            tester.expect_comments((), 1).await;

            tester.get_pr_copy(()).await.expect_priority(Some(10));

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn tree_closed_with_priority(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors treeclosed=5").await?;
            insta::assert_snapshot!(
                tester.get_next_comment_text(()).await?,
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

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn insufficient_permission_tree_closed(pool: sqlx::PgPool) {
        let gh = GitHub::default();
        gh.default_repo().lock().permissions = Permissions::empty();

        BorsBuilder::new(pool)
            .github(gh)
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors treeclosed=5").await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(())
            })
            .await;
    }

    fn review_comment(text: &str) -> Comment {
        Comment::from(text).with_author(User::reviewer())
    }

    #[sqlx::test]
    async fn cannot_approve_without_delegation(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors r+").await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_author(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @r#"
                :v: @default-user, you can now approve this pull request!

                If @reviewer told you to "`r=me`" after making some further change, then please make that change and post `@bors r=reviewer`.
                "#
                );

                tester
                    .get_pr_copy(())
                    .await
                    .expect_delegated(DelegatedPermission::Review);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_approve(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.approve(()).await?;

                tester
                    .get_pr_copy(())
                    .await
                    .expect_approved_by(&User::default_pr_author().name);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegatee_can_try(pool: sqlx::PgPool) {
        let gh = BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;
                tester.post_comment("@bors try").await?;
                tester.expect_comments((), 1).await;

                Ok(())
            })
            .await;
        gh.check_sha_history(
            default_repo_name(),
            TRY_MERGE_BRANCH_NAME,
            &["main-sha1", "merge-0-pr-1"],
        );
        gh.check_sha_history(default_repo_name(), TRY_BRANCH_NAME, &["merge-0-pr-1"]);
    }

    #[sqlx::test]
    async fn delegatee_can_set_priority(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.post_comment("@bors p=7").await?;
                tester.wait_for_pr((), |pr| pr.priority == Some(7)).await?;

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_insufficient_permission(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester.post_comment("@bors delegate+").await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_by_reviewer(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;
                tester
                    .get_pr_copy(())
                    .await
                    .expect_delegated(DelegatedPermission::Review);

                tester
                    .post_comment(review_comment("@bors delegate-"))
                    .await?;
                tester
                    .wait_for_pr((), |pr| pr.delegated_permission.is_none())
                    .await?;

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn undelegate_by_delegatee(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.post_comment("@bors delegate-").await?;
                tester
                    .wait_for_pr((), |pr| pr.delegated_permission.is_none())
                    .await?;

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn reviewer_unapprove_delegated_approval(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.approve(()).await?;
                tester
                    .get_pr_copy(())
                    .await
                    .expect_approved_by(&User::default_pr_author().name);

                tester.post_comment(review_comment("@bors r-")).await?;
                tester.expect_comments((), 1).await;

                tester.get_pr_copy(()).await.expect_unapproved();

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn non_author_cannot_use_delegation(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate+"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester
                    .post_comment(Comment::from("@bors r+").with_author(User::unprivileged()))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.get_pr_copy(()).await.expect_unapproved();

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegate_insufficient_permission_try_user(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(Comment::from("@bors delegate+").with_author(User::try_user()))
                    .await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @"@user-with-try-privileges: :key: Insufficient privileges: not in review users"
                );
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ rollup=never").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Never))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_rollup_bare_maybe(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ rollup-").await?;
            tester.expect_comments((), 1).await;
            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Maybe))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_with_priority_rollup(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r+ p=10 rollup=never").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_priority(Some(10))
                .expect_rollup(Some(RollupMode::Never))
                .expect_approved_by(&User::default_pr_author().name);
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_bare(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r=user1 rollup").await?;
            tester.expect_comments((), 1).await;
            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors r=user1 rollup=always").await?;
            tester.expect_comments((), 1).await;
            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_on_behalf_with_priority_rollup_value(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment("@bors r=user1 rollup=always priority=10")
                .await?;
            tester.expect_comments((), 1).await;
            tester
                .get_pr_copy(())
                .await
                .expect_priority(Some(10))
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by("user1");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn set_rollup_default(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors rollup").await?;
            tester
                .wait_for_pr((), |pr| pr.rollup == Some(RollupMode::Always))
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn set_rollup_with_value(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors rollup=maybe").await?;
            tester
                .wait_for_pr((), |pr| pr.rollup == Some(RollupMode::Maybe))
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_preserved_after_approve(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors rollup").await?;
            tester
                .wait_for_pr((), |pr| pr.rollup == Some(RollupMode::Always))
                .await?;

            tester.approve(()).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn rollup_overridden_on_approve_with_rollup(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors rollup=never").await?;
            tester
                .wait_for_pr((), |pr| pr.rollup == Some(RollupMode::Never))
                .await?;

            tester.post_comment("@bors r+ rollup").await?;
            tester.expect_comments((), 1).await;

            tester
                .get_pr_copy(())
                .await
                .expect_rollup(Some(RollupMode::Always))
                .expect_approved_by(&User::default_pr_author().name);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_store_sha(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.get_pr_copy(()).await.get_gh_pr();
            tester.approve(()).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_approved_sha(&pr.head_sha);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn reapproved_pr_uses_latest_sha(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            let pr = tester.get_pr_copy(()).await.get_gh_pr();
            tester.approve(()).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_approved_sha(&pr.head_sha);

            tester.push_to_pr(()).await?;
            let pr2 = tester.get_pr_copy(()).await.get_gh_pr();
            assert_ne!(pr.head_sha, pr2.head_sha);

            tester.expect_comments((), 1).await;

            tester.approve(()).await?;

            tester
                .get_pr_copy(())
                .await
                .expect_approved_sha(&pr2.head_sha);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn delegate_try(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate=try"))
                    .await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @r"
                :v: @default-user, you can now perform try builds on this pull request!

                You can now post `@bors try` to start a try build.
                "
                );
                tester
                    .get_pr_copy(())
                    .await
                    .expect_delegated(DelegatedPermission::Try);
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegated_try_can_build(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate=try"))
                    .await?;
                tester.expect_comments((), 1).await;

                tester.post_comment("@bors try").await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
                :hourglass: Trying commit pr-1-sha with merge merge-0-pr-1…

                To cancel the try build, run the command `@bors try cancel`.
                ");

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn delegated_try_can_not_approve(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::unauthorized_pr_author())
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .post_comment(review_comment("@bors delegate=try"))
                    .await?;
                tester.expect_comments((), 1).await;
                tester
                    .get_pr_copy(())
                    .await
                    .expect_delegated(DelegatedPermission::Try);

                tester.post_comment("@bors r+").await?;
                insta::assert_snapshot!(
                    tester.get_next_comment_text(()).await?,
                    @"@default-user: :key: Insufficient privileges: not in review users"
                );
                tester.get_pr_copy(()).await.expect_unapproved();

                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn multiple_commands_in_one_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .post_comment(
                    r#"
@bors r+ rollup=never
@bors p=10
"#,
                )
                .await?;
            tester.expect_comments((), 1).await;

            tester
                .wait_for_pr((), |pr| {
                    pr.rollup == Some(RollupMode::Never) && pr.priority == Some(10)
                })
                .await?;
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_draft_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .set_pr_status_draft(())
                .await?;
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":clipboard: Only open, non-draft PRs can be approved.");
            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_closed_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .set_pr_status_closed(())
                .await?;
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":clipboard: Only open, non-draft PRs can be approved.");
            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn approve_merged_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .set_pr_status_merged(())
                .await?;
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":clipboard: Only open, non-draft PRs can be approved.");
            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn approve_wip_pr(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester
                .edit_pr((), |pr| {
                    pr.title = "[do not merge] CI experiments".to_string();
                })
                .await?;
            tester.post_comment("@bors r+").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            :clipboard: Looks like this PR is still in progress, ignoring approval.

            Hint: Remove **[do not merge]** from this PR's title when it is ready for review.
            ");
            tester.get_pr_copy(()).await.expect_unapproved();
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn approve_pr_with_blocked_label(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
labels_blocking_approval = ["proposed-final-comment-period"]
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .edit_pr((), |pr| {
                        pr.labels = vec![
                            "S-waiting-on-review".to_string(),
                            "proposed-final-comment-period".to_string(),
                        ];
                    })
                    .await?;
                tester.post_comment("@bors r+").await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":clipboard: This PR cannot be approved because it currently has the following label: `proposed-final-comment-period`.");
                tester.get_pr_copy(()).await.expect_unapproved();
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn approve_pr_with_blocked_labels(pool: sqlx::PgPool) {
        BorsBuilder::new(pool)
            .github(GitHub::default().with_default_config(
                r#"
labels_blocking_approval = ["proposed-final-comment-period", "final-comment-period"]
"#,
            ))
            .run_test(async |tester: &mut BorsTester| {
                tester
                    .edit_pr((), |pr| {
                        pr.labels = vec![
                            "S-waiting-on-review".to_string(),
                            "proposed-final-comment-period".to_string(),
                            "final-comment-period".to_string(),
                            "S-blocked".to_string(),
                        ];
                    })
                    .await?;
                tester.post_comment("@bors r+").await?;
                insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @":clipboard: This PR cannot be approved because it currently has the following labels: `proposed-final-comment-period`, `final-comment-period`.");
                tester.get_pr_copy(()).await.expect_unapproved();
                Ok(())
            })
            .await;
    }

    #[sqlx::test]
    async fn unapprove_running_auto_build_pr_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.get_pr_copy(()).await.expect_auto_build(|_| true);
            tester.workflow_start(tester.auto_branch()).await?;
            tester.post_comment("@bors r-").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
                Commit pr-1-sha has been unapproved.

                Auto build cancelled due to unapproval. Cancelled workflows:

                - https://github.com/rust-lang/borstest/actions/runs/1
                ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_running_auto_build_pr_failed_comment(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.with_repo((), |pr| pr.workflow_cancel_error = true);

            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.get_pr_copy(()).await.expect_auto_build(|_| true);
            tester.workflow_start(tester.auto_branch()).await?;
            tester.post_comment("@bors r-").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            Commit pr-1-sha has been unapproved.

            Auto build cancelled due to unapproval. It was not possible to cancel some workflows.
            ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unapprove_running_auto_build_updates_check_run(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.approve(()).await?;
            tester.start_auto_build(()).await?;
            tester.get_pr_copy(()).await.expect_auto_build(|_| true);

            tester.workflow_start(tester.auto_branch()).await?;
            tester.post_comment("@bors r-").await?;
            tester.expect_comments((), 1).await;
            tester.expect_check_run(
                &tester.get_pr_copy(()).await.get_gh_pr().head_sha,
                AUTO_BUILD_CHECK_RUN_NAME,
                AUTO_BUILD_CHECK_RUN_NAME,
                CheckRunStatus::Completed,
                Some(CheckRunConclusion::Cancelled),
            );
            Ok(())
        })
        .await;
    }
}
