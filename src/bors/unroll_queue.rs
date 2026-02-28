use crate::bors::build::{
    BuildStartError, StartBuildCommitStrategy, StartBuildContext, StartBuildOutcome, start_build,
};
use crate::bors::comment::rollup_perf_table_comment;
use crate::bors::{BuildKind, PullRequestStatus, RepositoryState, TRY_PERF_BRANCH_NAME};
use crate::database::{
    ExclusiveLockProof, ExclusiveOperationOutcome, PullRequestModel, RollupMemberModel,
};
use crate::github::{CommitSha, GithubRepoName, PullRequest, PullRequestNumber};
use crate::{BorsContext, PgDbClient};
use anyhow::Context;
use regex::Regex;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::LazyLock;
use tokio::sync::mpsc;

/// Branch used for preparing try-perf merge commits.
/// It should not run CI checks.
pub(super) const TRY_PERF_MERGE_BRANCH_NAME: &str = "automation/bors/try-perf-merge";

pub type UnrollQueueReceiver = mpsc::Receiver<UnrollQueueEvent>;

#[derive(Debug)]
pub enum UnrollQueueEvent {
    /// Refresh pending rollups for a repository from the DB and process them.
    RefreshPendingRollups(GithubRepoName),
    /// Enqueue a specific rollup for unroll processing.
    ProcessRollup {
        repo: GithubRepoName,
        rollup: PullRequestNumber,
    },
}

#[derive(Clone)]
pub struct UnrollQueueSender {
    inner: mpsc::Sender<UnrollQueueEvent>,
}

impl UnrollQueueSender {
    /// Refresh pending rollups for a repository.
    pub async fn refresh_pending_rollups(
        &self,
        repo: GithubRepoName,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(UnrollQueueEvent::RefreshPendingRollups(repo))
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    /// Enqueue a rollup for unroll processing.
    pub async fn enqueue_rollup(
        &self,
        repo: GithubRepoName,
        rollup: PullRequestNumber,
    ) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(UnrollQueueEvent::ProcessRollup { repo, rollup })
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }
}

pub fn create_unroll_queue() -> (UnrollQueueSender, UnrollQueueReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    (UnrollQueueSender { inner: tx }, rx)
}

pub async fn handle_unroll_queue_event(
    ctx: Arc<BorsContext>,
    event: UnrollQueueEvent,
) -> anyhow::Result<()> {
    match event {
        UnrollQueueEvent::RefreshPendingRollups(repo) => {
            let repo = ctx.get_repo(&repo)?;
            let unresolved_rollups = ctx.db.get_pending_rollup_unrolls(repo.repository()).await?;
            handle_repository_unroll(&ctx.db, &repo, unresolved_rollups).await?;
        }
        UnrollQueueEvent::ProcessRollup { repo, rollup } => {
            let repo = ctx.get_repo(&repo)?;
            handle_repository_unroll(&ctx.db, &repo, vec![rollup]).await?;
        }
    }

    Ok(())
}

static ROLLEDUP_PR_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^Rollup merge of #(\d+)").unwrap());

// Get the member PR number from a rollup commit message
fn parse_rollup_member_number(message: &str) -> Option<PullRequestNumber> {
    ROLLEDUP_PR_NUMBER
        .captures(message)
        .and_then(|captures| captures.get(1))
        .and_then(|number| number.as_str().parse::<u64>().ok())
        .map(PullRequestNumber)
}

async fn handle_repository_unroll(
    db: &PgDbClient,
    repo: &RepositoryState,
    rollups: Vec<PullRequestNumber>,
) -> anyhow::Result<()> {
    let repo_name = repo.repository();
    let res = db
        .ensure_not_concurrent(&format!("{repo_name}-unroll"), async |proof| {
            'process_rollups: for rollup_number in rollups {
                let Some(rollup) = db.get_pull_request(repo_name, rollup_number).await? else {
                    continue;
                };

                let members = db.get_rollup_members(rollup.id).await?;
                let rollup_commits = repo.client.get_pull_request_commits(rollup.number).await?;
                // Get all the rollup numbers and their original rollup commit messages
                let member_rollup_messages = rollup_commits
                    .into_iter()
                    .filter_map(|commit| {
                        parse_rollup_member_number(&commit.message)
                            .map(|number| (number, commit.message))
                    })
                    .collect::<HashMap<_, _>>();

                for member in &members {
                    // This member was already handled in an earlier pass
                    if member.unrolled_commit.is_some() {
                        continue;
                    }

                    let Some(rollup_merge_message) =
                        member_rollup_messages.get(&member.member_number)
                    else {
                        tracing::warn!(
                            "Could not find rollup merge message for rollup #{rollup_number} member #{}",
                            member.member_number
                        );
                        continue;
                    };

                    match handle_start_perf_build(
                        repo,
                        db,
                        &rollup,
                        member,
                        rollup_merge_message,
                        &proof,
                    )
                    .await?
                    {
                        PerfBuildStartOutcome::BuildStarted => {}
                        PerfBuildStartOutcome::ContinueToNextMember => {}
                        PerfBuildStartOutcome::PauseQueue => {
                            break 'process_rollups;
                        }
                    }
                }

                let members = db.get_rollup_members(rollup.id).await?;
                let remaining_members = members
                    .iter()
                    .filter(|member| !member.has_unroll_concluded());

                // Only post the comment if all members have concluded
                if members.is_empty() || remaining_members.count() != 0 {
                    continue;
                }

                let base_sha = rollup
                    .auto_build
                    .as_ref()
                    .map(|build| CommitSha(build.parent.clone()))
                    .unwrap_or(CommitSha("unknown".to_string()));
                let member_prs = db.get_rollup_member_prs(rollup.id).await?;

                let comment = rollup_perf_table_comment(
                    repo.repository(),
                    &base_sha,
                    &members,
                    &member_prs,
                );
                if let Err(error) = repo.client.post_comment(rollup_number, comment, db).await {
                    tracing::error!(
                        "Failed to post unroll table comment for #{rollup_number}: {error:?}",
                    );
                }
            }

            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("Unroll lock failure")?;

    match res {
        ExclusiveOperationOutcome::Performed(_) => {}
        ExclusiveOperationOutcome::Skipped => {
            tracing::warn!("Unroll queue was not performed due to other concurrent bors instance");
        }
    }

    Ok(())
}

#[derive(Debug)]
enum SanityCheckError {
    /// The pull request is not a rollup in the DB.
    NotRollup,
    /// The pull request is not merged in GitHub (yet).
    NotMerged,
}

fn sanity_check_rollup(is_rollup: bool, gh_rollup: &PullRequest) -> Result<(), SanityCheckError> {
    if !is_rollup {
        return Err(SanityCheckError::NotRollup);
    }

    if gh_rollup.status != PullRequestStatus::Merged {
        return Err(SanityCheckError::NotMerged);
    }

    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerfBuildStartOutcome {
    /// A try-perf build was successfully started for this rollup member.
    BuildStarted,
    /// This member was handled (or skipped), continue with the next member.
    ContinueToNextMember,
    /// There was some transient error, the queue should skip this rollup.
    PauseQueue,
}

#[tracing::instrument(skip(repo, db, rollup, member, proof))]
async fn handle_start_perf_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    rollup: &PullRequestModel,
    member: &RollupMemberModel,
    rollup_merge_message: &str,
    proof: &ExclusiveLockProof,
) -> anyhow::Result<PerfBuildStartOutcome> {
    let rollup_number = rollup.number;
    let member_number = member.member_number;

    let Err(error) = start_perf_build(repo, db, rollup, member, rollup_merge_message, proof).await
    else {
        tracing::info!(
            "Started try-perf build for rollup #{rollup_number} member #{member_number}"
        );
        return Ok(PerfBuildStartOutcome::BuildStarted);
    };

    match error {
        StartPerfBuildError::MergeConflict => Ok(PerfBuildStartOutcome::ContinueToNextMember),
        StartPerfBuildError::SanityCheckFailed {
            error: SanityCheckError::NotRollup,
        } => {
            tracing::debug!(
                "Skipping try-perf build for rollup #{rollup_number} member #{member_number} because it is not a rollup in DB"
            );
            Ok(PerfBuildStartOutcome::ContinueToNextMember)
        }
        StartPerfBuildError::SanityCheckFailed {
            error: SanityCheckError::NotMerged,
        } => {
            tracing::debug!(
                "Pausing unroll queue after try-perf build for rollup #{rollup_number} member #{member_number}: rollup is not merged on GitHub yet"
            );
            // Likely a transient error as GitHub has not yet updated its state
            Ok(PerfBuildStartOutcome::PauseQueue)
        }
        StartPerfBuildError::DatabaseError(error) => {
            tracing::warn!(
                "Failed to start try-perf build for rollup #{rollup_number} member #{member_number} due to database error: {error:?}",
            );
            Ok(PerfBuildStartOutcome::PauseQueue)
        }
        StartPerfBuildError::GitHubError(error) => {
            tracing::warn!(
                "Failed to start try-perf build for rollup #{rollup_number} member #{member_number} due to GitHub error: {error:?}",
            );
            Ok(PerfBuildStartOutcome::PauseQueue)
        }
    }
}

#[must_use]
enum StartPerfBuildError {
    /// Member cannot be merged on top of the rollup base.
    MergeConflict,
    /// Sanity checks failed - rollup state doesn't match requirements.
    SanityCheckFailed { error: SanityCheckError },
    /// Failed to perform required database operation.
    DatabaseError(anyhow::Error),
    /// GitHub API error.
    GitHubError(anyhow::Error),
}

async fn start_perf_build(
    repo: &RepositoryState,
    db: &PgDbClient,
    rollup: &PullRequestModel,
    member: &RollupMemberModel,
    rollup_merge_message: &str,
    proof: &ExclusiveLockProof,
) -> Result<(), StartPerfBuildError> {
    let is_rollup = db
        .is_rollup(rollup)
        .await
        .map_err(StartPerfBuildError::DatabaseError)?;
    let gh_rollup = repo
        .client
        .get_pull_request(rollup.number)
        .await
        .map_err(StartPerfBuildError::GitHubError)?;
    sanity_check_rollup(is_rollup, &gh_rollup)
        .map_err(|error| StartPerfBuildError::SanityCheckFailed { error })?;

    let base_sha = rollup
        .auto_build
        .as_ref()
        .map(|build| CommitSha(build.parent.clone()))
        .unwrap_or(CommitSha("unknown".to_string()));
    let merge_message = format!(
        "Unrolled build for #{}\n{rollup_merge_message}",
        member.member_number.0
    );

    let build_commit_result = start_build(
        db,
        repo,
        proof,
        StartBuildContext {
            merge_branch: TRY_PERF_MERGE_BRANCH_NAME.to_string(),
            ci_branch: TRY_PERF_BRANCH_NAME.to_string(),
            base_sha: base_sha.clone(),
            head_sha: CommitSha(member.rolled_up_sha.clone()),
            build_kind: BuildKind::TryPerf,
        },
        StartBuildCommitStrategy::UseMergeCommit {
            message: merge_message,
        },
        None,
        None,
    )
    .await
    .map_err(|error| match error {
        BuildStartError::GithubError(error) => StartPerfBuildError::GitHubError(error),
        BuildStartError::DatabaseError(error) => StartPerfBuildError::DatabaseError(error),
    })?;

    let build_id = match build_commit_result {
        StartBuildOutcome::Success {
            build_commit_sha: _,
            build_id,
        } => build_id,
        StartBuildOutcome::MergeConflict => {
            // Record the unrolled commit with no build so we know it's processed
            db.create_unrolled_commit(rollup.id, member.member_id, None)
                .await
                .map_err(StartPerfBuildError::DatabaseError)?;
            return Err(StartPerfBuildError::MergeConflict);
        }
    };

    db.create_unrolled_commit(rollup.id, member.member_id, Some(build_id))
        .await
        .map_err(StartPerfBuildError::DatabaseError)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::TRY_PERF_MERGE_BRANCH_NAME;
    use crate::bors::TRY_PERF_BRANCH_NAME;
    use crate::github::rollup::OAuthRollupState;
    use crate::github::{GithubRepoName, PullRequestNumber};
    use crate::permissions::PermissionType;
    use crate::tests::default_repo_name;
    use crate::tests::{
        ApiRequest, ApiResponse, BorsTester, GitHub, MergeBehavior, PullRequest, Repo, User,
        run_test,
    };
    use http::StatusCode;

    #[sqlx::test]
    async fn unroll_succeeds_comment(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3], |_| {}).await?;
            ctx.wait_for_unroll_queue().await;

            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.wait_for_unroll_queue().await;
            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.wait_for_unroll_queue().await;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @r"
            📌 Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#2|Title of PR 2|`merge-0-pr-2-f836d7c8`<br>([link](https://github.com/rust-lang/borstest/commit/merge-0-pr-2-f836d7c8))|
            |#3|Title of PR 3|`merge-1-pr-3-b409ae01`<br>([link](https://github.com/rust-lang/borstest/commit/merge-1-pr-3-b409ae01))|

            *previous master*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_perf_build_commit_message(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2], |_| {}).await?;
            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.expect_comments(rollup, 1).await;

            insta::assert_snapshot!(ctx.try_perf_branch().get_commit().message(), @"
            Unrolled build for #2
            Rollup merge of #2 - default-user:pr/2, r=default-user

            Title of PR 2

            Description of PR 2
            ");

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_merge_conflict_comment(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3], |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Cause a conflict on the first member merge
                    (n == 2).then_some(StatusCode::CONFLICT)
                }));
            })
            .await?;

            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @r"
            📌 Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#2|Title of PR 2|`merge-0-pr-2-f836d7c8`<br>([link](https://github.com/rust-lang/borstest/commit/merge-0-pr-2-f836d7c8))|
            |#3|Title of PR 3|❌ conflicts merging '[pr-3-sha](https://github.com/rust-lang/borstest/commit/pr-3-sha)' into previous master ❌|

            *previous master*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_perf_build_branch_history(pool: sqlx::PgPool) {
        let gh = run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3], |_| {}).await?;
            ctx.wait_for_unroll_queue().await;
            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.wait_for_unroll_queue().await;
            ctx.workflow_full_success(ctx.try_perf_workflow()).await?;
            ctx.wait_for_unroll_queue().await;
            ctx.expect_comments(rollup, 1).await;

            Ok(())
        })
        .await;

        insta::assert_snapshot!(gh.get_sha_history(
            (),
            TRY_PERF_MERGE_BRANCH_NAME,
        ), @"
        main-sha1
        merge-0-pr-2-f836d7c8
        main-sha1
        merge-1-pr-3-b409ae01
        ");
        insta::assert_snapshot!(gh.get_sha_history(
            (),
            TRY_PERF_BRANCH_NAME,
        ), @"
        merge-0-pr-2-f836d7c8
        merge-1-pr-3-b409ae01
        ");
    }

    #[sqlx::test]
    async fn unroll_merge_conflict_records_unrolled_commit_without_build(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2], |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Cause a conflict on the first member merge
                    (n == 1).then_some(StatusCode::CONFLICT)
                }));
            })
            .await?;
            ctx.wait_for_unroll_queue().await;
            ctx.expect_comments(rollup, 1).await;

            let rollup_db = ctx
                .db()
                .get_pull_request(&default_repo_name(), rollup)
                .await?
                .expect("Rollup not found");
            let rollup_members = ctx.db().get_rollup_members(rollup_db.id).await?;
            assert!(
                rollup_members
                    .first()
                    .expect("Rollup should have one member")
                    .unrolled_commit
                    .as_ref()
                    .expect("Missing unrolled commit")
                    .build
                    .is_none()
            );

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_inserts_member_build(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2], |_| {}).await?;
            ctx.refresh_pending_unrolls().await;

            let rollup_db = ctx
                .db()
                .get_pull_request(&default_repo_name(), rollup)
                .await?
                .expect("Rollup not found");
            let rollup_members = ctx.db().get_rollup_members(rollup_db.id).await?;
            assert!(
                rollup_members
                    .first()
                    .expect("Rollup should have one member")
                    .unrolled_commit
                    .as_ref()
                    .expect("Missing unrolled commit")
                    .build
                    .is_some()
            );
            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn recover_unroll_transient_error(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3], |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Fail from second merge onwards
                    (n >= 2).then_some(StatusCode::INTERNAL_SERVER_ERROR)
                }));
            })
            .await?;
            ctx.wait_for_unroll_queue().await;

            let rollup_db = ctx
                .db()
                .get_pull_request(&default_repo_name(), rollup)
                .await?
                .expect("Rollup not found");
            let members = ctx.db().get_rollup_members(rollup_db.id).await?;
            let member2 = members
                .iter()
                .find(|member| member.member_number == pr2.number())
                .expect("PR should be in rollup");
            let member3 = members
                .iter()
                .find(|member| member.member_number == pr3.number())
                .expect("PR should be in rollup");

            assert!(
                member2
                    .unrolled_commit
                    .as_ref()
                    .expect("Missing unrolled commit")
                    .build
                    .is_some()
            );
            assert!(member3.unrolled_commit.is_none());

            ctx.modify_repo((), |repo| {
                repo.merge_behavior = MergeBehavior::Succeed;
            });
            ctx.refresh_pending_unrolls().await;
            ctx.refresh_pending_unrolls().await;

            let members = ctx.db().get_rollup_members(rollup_db.id).await?;
            assert_eq!(members.len(), 2);
            assert!(members.iter().all(|member| {
                member
                    .unrolled_commit
                    .as_ref()
                    .and_then(|unrolled| unrolled.build.as_ref())
                    .is_some()
            }));

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn try_perf_build_concurrent_bors_instances(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3], |_| {}).await?;

            let concurrent_futs = (0..10)
                .map(|_| async { ctx.refresh_pending_unrolls().await })
                .collect::<Vec<_>>();
            futures::future::join_all(concurrent_futs).await;

            let rollup_db = ctx
                .db()
                .get_pull_request(&default_repo_name(), rollup)
                .await?
                .expect("Rollup not found");
            let members = ctx.db().get_rollup_members(rollup_db.id).await?;

            assert_eq!(members.len(), 2);
            assert!(
                members
                    .iter()
                    .all(|member| member.unrolled_commit.is_some())
            );
            assert_eq!(ctx.try_perf_branch().get_commit_history().len(), 2);

            Ok(())
        })
        .await;
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

    async fn make_merged_rollup(
        ctx: &mut BorsTester,
        prs: &[&PullRequest],
        modify_repo: impl FnOnce(&mut Repo),
    ) -> anyhow::Result<PullRequestNumber> {
        let response = make_rollup(ctx, prs).await?;
        let location = response.get_header("location");
        let rollup: u64 = location
            .rsplit('/')
            .next()
            .ok_or_else(|| anyhow::anyhow!("Invalid rollup redirect URL: {location}"))?
            .parse()?;
        let pr_number = PullRequestNumber(rollup);
        ctx.approve(pr_number).await?;
        ctx.start_auto_build(pr_number).await?;
        ctx.modify_repo((), modify_repo);
        // We need GH to report the rollup as merged for unroll sanity checks,
        // but cannot send the merged webhook here because it would set DB status
        // to merged before `finish_auto_build` finalizes the merge.
        ctx.modify_pr_in_gh(pr_number, |pr| pr.merge());
        ctx.finish_auto_build(pr_number).await?;
        Ok(pr_number)
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
            repo_name: repo.name().to_string(),
            repo_owner: repo.owner().to_string(),
        };
        ApiRequest::get("/oauth/callback")
            .query("code", code)
            .query("state", &serde_json::to_string(&state).unwrap())
    }
}
