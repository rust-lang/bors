use crate::bors::event::{PullRequestEdited, PullRequestOpened, PullRequestPushed};
use crate::bors::handlers::labels::handle_label_trigger;
use crate::bors::{Comment, RepositoryState};
use crate::github::{CommitSha, LabelTrigger, PullRequestNumber};
use crate::PgDbClient;
use std::sync::Arc;

pub(super) async fn handle_pull_request_edited(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestEdited,
) -> anyhow::Result<()> {
    let pr = &payload.pull_request;
    let pr_number = pr.number;

    // If the base branch has changed, unapprove the PR
    let Some(_) = payload.from_base_sha else {
        return Ok(());
    };

    let pr_model = db
        .get_or_create_pull_request(repo_state.repository(), pr_number, &pr.base.name)
        .await?;
    db.update_pr_base_branch(&pr_model, &pr.base.name).await?;
    if !pr_model.is_approved() {
        return Ok(());
    }

    db.unapprove(&pr_model).await?;
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
        .get_or_create_pull_request(repo_state.repository(), pr.number, &pr.base.name)
        .await?;

    if !pr_model.is_approved() {
        return Ok(());
    }

    let pr_number = pr_model.number;
    db.unapprove(&pr_model).await?;
    handle_label_trigger(&repo_state, pr_number, LabelTrigger::Unapproved).await?;
    notify_of_pushed_pr(&repo_state, pr_number, pr.head.sha.clone()).await
}

pub(super) async fn handle_pull_request_opened(
    repo_state: Arc<RepositoryState>,
    db: Arc<PgDbClient>,
    payload: PullRequestOpened,
) -> anyhow::Result<()> {
    db.create_pull_request(
        repo_state.repository(),
        payload.pull_request.number,
        &payload.pull_request.base.name,
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
    use crate::tests::mocks::{
        assert_pr_approved_by, assert_pr_unapproved, create_world_with_approve_config,
        default_branch_name, default_branch_sha, default_pr_number, run_test, BorsBuilder,
        GitHubPullRequest, PullRequestChangeEvent, User,
    };

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
                            from_base_ref: Some("main".to_string()),
                        },
                    )
                    .await?;

                assert_eq!(
                    tester.get_comment().await?,
                    r#":warning: The base branch changed to `main`, and the
PR will need to be re-approved."#,
                );
                assert_pr_unapproved(&tester, default_pr_number().into()).await;
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
                            from_base_ref: None,
                        },
                    )
                    .await?;

                assert_pr_approved_by(
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
                        from_base_ref: Some("main".to_string()),
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
                assert_pr_unapproved(&tester, default_pr_number().into()).await;
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

    #[sqlx::test]
    async fn update_base_branch_on_pr_opened(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.open_pr(default_pr_number()).await?;
            tester
                .wait_for(|| async {
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.base_branch == "main".to_string())
                })
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[tracing_test::traced_test]
    #[sqlx::test]
    async fn update_base_branch_on_pr_edited(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .edit_pull_request_with_pr(
                    default_pr_number(),
                    PullRequestChangeEvent {
                        from_base_sha: Some(default_branch_sha().to_string()),
                        from_base_ref: Some(default_branch_name().to_string()),
                    },
                    GitHubPullRequest::new(default_pr_number())
                        .with_base("new".to_string(), "new-sha".to_string()),
                )
                .await?;
            tester
                .wait_for(|| async {
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.base_branch == "new".to_string())
                })
                .await?;
            Ok(tester)
        })
        .await;
    }

    #[sqlx::test]
    async fn preserve_base_branch_on_pr_edited_when_base_not_edited(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester
                .edit_pull_request(
                    default_pr_number(),
                    PullRequestChangeEvent {
                        from_base_sha: None,
                        from_base_ref: None,
                    },
                )
                .await?;

            tester
                .wait_for(|| async {
                    let pr = tester.get_default_pr().await?;
                    Ok(pr.base_branch == "main".to_string())
                })
                .await?;
            Ok(tester)
        })
        .await;
    }
}
