use crate::bors::build::{
    StartBuildCommit, StartBuildContext, StartBuildError, StartBuildOutcome, start_build,
};
use crate::bors::{BuildKind, Comment, RepositoryState, TRY_PERF_BRANCH_NAME, bors_commit_author};
use crate::database::{
    BuildModel, BuildStatus, ExclusiveOperationOutcome, PullRequestModel, RollupMemberForUnrolling,
    UnrollState,
};
use crate::github::{CommitSha, GithubRepoName, PullRequestNumber};
use crate::{BorsContext, PgDbClient};
use anyhow::Context;
use std::collections::HashMap;
use std::fmt::Write;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{Instrument, info_span};

// This branch serves for preparing the final commit.
// It will be reset to master and merged with the branch that should be tested.
// Because this action (reset + merge) is not atomic, this branch should not run CI checks to avoid
// starting them twice.
const TRY_PERF_MERGE_BRANCH_NAME: &str = "automation/bors/try-perf-merge";

pub type UnrollQueueReceiver = mpsc::Receiver<UnrollQueueEvent>;

#[derive(Debug)]
pub enum UnrollQueueEvent {
    ProcessUnrolledMembers(GithubRepoName),
}

#[derive(Clone)]
pub struct UnrollQueueSender {
    inner: mpsc::Sender<UnrollQueueEvent>,
}

impl UnrollQueueSender {
    pub async fn process_unrolled_members(
        &self,
        repo: &GithubRepoName,
    ) -> Result<(), mpsc::error::SendError<UnrollQueueEvent>> {
        self.inner
            .send(UnrollQueueEvent::ProcessUnrolledMembers(repo.clone()))
            .await
    }
}

pub fn create_unroll_queue() -> (UnrollQueueSender, UnrollQueueReceiver) {
    let (tx, rx) = tokio::sync::mpsc::channel(1024);
    (UnrollQueueSender { inner: tx }, rx)
}

/// This function starts unrolled builds for rollup members,
/// and sends a final comment to the rollup once all the builds are finished.
///
/// In theory, this could be implemented within the build queue, but there are some reasons why
/// not to do that:
/// - Unrolling operations should not interfere with build queue operations, which are more
///   important. This is particularly important for starting the unrolled builds, which can take a
///   long time for large rollups.
/// - We can explicitly trigger it in tests without also triggering the build queue.
///
/// We implement this as a separate unroll queue, so that it does not block other background sync
/// processes, and so that we can easily trigger it via a dedicated queue
pub async fn handle_unroll_queue_event(
    ctx: Arc<BorsContext>,
    event: UnrollQueueEvent,
) -> anyhow::Result<()> {
    let UnrollQueueEvent::ProcessUnrolledMembers(repo) = event;
    let Ok(repo) = ctx.get_repo(&repo) else {
        return Err(anyhow::anyhow!("Repo {repo} not found"));
    };

    if repo.config.load().unroll.is_none() {
        return Ok(());
    }

    let db = &ctx.db;
    let span = info_span!(
        "Processing unrolled builds",
        repo = repo.repository().to_string()
    );
    process_unrolled_members(&repo, db).instrument(span).await?;

    Ok(())
}

async fn process_unrolled_members(repo: &RepositoryState, db: &PgDbClient) -> anyhow::Result<()> {
    // Find all unrolled members that have not been processed yet
    let members: Vec<RollupMemberForUnrolling> = db
        .get_rollup_members_for_unrolling(repo.repository())
        .await?;
    if members.is_empty() {
        return Ok(());
    }

    tracing::info!(
        "Found {} rollup members for unroll processing",
        members.len()
    );

    // Group members by their rollup
    let rollups: HashMap<i32, Vec<RollupMemberForUnrolling>> =
        members
            .into_iter()
            .fold(HashMap::new(), |mut rollups, member| {
                rollups
                    .entry(member.member.rollup_id)
                    .or_default()
                    .push(member);
                rollups
            });

    for (rollup_id, members) in rollups {
        let Some(rollup) = db.get_pull_request_by_id(rollup_id).await? else {
            tracing::error!("Rollup with ID {rollup_id} could not be found in the database");
            // The rollup is somehow missing in the DB, finish all its members
            db.set_all_rollup_members_state(rollup_id, UnrollState::Reported)
                .await?;
            continue;
        };
        let rollup_number = rollup.number;
        let Some(rollup_auto_build) = &rollup.auto_build else {
            tracing::error!("Rollup #{rollup_number} has no auto build attached");
            db.set_all_rollup_members_state(rollup_id, UnrollState::Reported)
                .await?;
            continue;
        };

        tracing::info!(
            "Processing {} unrolled members of rollup #{rollup_number}",
            members.len(),
        );
        let span = info_span!("Rollup unrolling", rollup = rollup_number.0);

        process_rollup(db, repo, &rollup, rollup_auto_build, &members)
            .instrument(span)
            .await
            .with_context(|| {
                anyhow::anyhow!("Transient error occurred while unrolling rollup #{rollup_number}")
            })?;
    }
    Ok(())
}

enum UnrollError {
    CommitNotFound { sha: CommitSha },
    MergeConflict,
    Transient(anyhow::Error),
}

impl From<anyhow::Error> for UnrollError {
    fn from(error: anyhow::Error) -> Self {
        Self::Transient(error)
    }
}

async fn process_rollup<'a>(
    db: &'a PgDbClient,
    repo: &'a RepositoryState,
    rollup: &PullRequestModel,
    rollup_auto_build: &BuildModel,
    members: &'a [RollupMemberForUnrolling],
) -> anyhow::Result<()> {
    let mut completed_members: HashMap<PullRequestNumber, CompletedMember> = HashMap::new();

    for member in members {
        let unroll_state = member.member.unroll_state.as_ref().unwrap_or_else(|| panic!("get_rollup_members_for_unrolling returned a rollup member {member:?} with NULL unroll state"));
        tracing::info!("Member #{} state: {unroll_state:?}", member.pr.number);

        match unroll_state {
            UnrollState::Waiting => {
                // No unrolled build started yet, start it
                let build_result =
                    start_unrolled_build(db, repo, rollup, rollup_auto_build, member).await;
                let merge_sha = match build_result {
                    Ok(sha) => sha,
                    Err(UnrollError::CommitNotFound { sha }) => {
                        tracing::error!(
                            "Merge commit {sha} for member {member:?} could not be found"
                        );
                        // Consider this member's unrolled build to be failed
                        db.set_rollup_member_state(&member.member, UnrollState::Finished)
                            .await?;
                        continue;
                    }
                    Err(UnrollError::MergeConflict) => {
                        tracing::error!(
                            "Merge conflict happened while creating an unrolled build for member {member:?}"
                        );
                        // Consider this member's unrolled build to be failed
                        db.set_rollup_member_state(&member.member, UnrollState::Finished)
                            .await?;
                        continue;
                    }
                    Err(UnrollError::Transient(error)) => {
                        return Err(error.context(format!("Rollup member {member:?}")));
                    }
                };
                tracing::info!("Started unrolled build with SHA `{merge_sha}`");

                // The build has been started, mark the member as pending
                // If the member is pending, there must always be a build present for it!
                db.set_rollup_member_state(&member.member, UnrollState::Pending)
                    .await?;
            }
            UnrollState::Pending => {
                let unrolled_build = member.pr.unrolled_build.as_ref().unwrap_or_else(|| {
                    panic!(
                        "Rollup member {member:?} has unroll state pending, but no attached unrolled build"
                    )
                });

                // If a pending build has finished in the meantime, mark it as such
                match unrolled_build.status {
                    BuildStatus::Pending => {}
                    BuildStatus::Success
                    | BuildStatus::Failure
                    | BuildStatus::Cancelled
                    | BuildStatus::Timeouted => {
                        completed_members.insert(
                            member.pr.number,
                            CompletedMember {
                                member,
                                build: Some(unrolled_build),
                            },
                        );
                        // Mark the member as finished
                        db.set_rollup_member_state(&member.member, UnrollState::Finished)
                            .await?;
                    }
                }
            }
            UnrollState::Finished => {
                let unrolled_build = member.pr.unrolled_build.as_ref();

                // This member was already previously completed
                completed_members.insert(
                    member.pr.number,
                    CompletedMember {
                        member,
                        build: unrolled_build,
                    },
                );
            }
            UnrollState::Reported => {
                // This should not happen...
                panic!(
                    "Encountered unprocessed rollup member {member:?} with unroll state reported"
                );
            }
        }
    }

    // All members are completed, finish the unrolling process
    if completed_members.len() == members.len() {
        tracing::info!("All rollup members processed, sending the comment");

        // Send comment
        let members: Vec<CompletedMember> = completed_members.into_values().collect();
        let parent_sha = CommitSha(rollup_auto_build.parent.clone());
        let comment =
            create_unroll_result_comment(repo.repository(), db, parent_sha, members).await;
        repo.client.post_comment(rollup.number, comment, db).await?;

        // If that succeeded, mark all members as reported
        // We cannot atomically send the comment and mark the members as reported.
        // GitHub failures are more common than DB failures, so we prefer sending the comment twice
        // in case of a DB error, rather than missing sending of the comment in case of a GitHub
        // error.
        db.set_all_rollup_members_state(rollup.id, UnrollState::Reported)
            .await?;

        tracing::info!("Finished unrolling");

        // At this point, the unrolling is finished and the given rollup and its members should
        // never go to this function again
    }

    Ok(())
}

async fn create_unroll_result_comment(
    repo: &GithubRepoName,
    db: &PgDbClient,
    parent_sha: CommitSha,
    mut members: Vec<CompletedMember<'_>>,
) -> Comment {
    // We want to sort the members by the order they occurred in the rollup
    members.sort_by_key(|v| v.member.member.position);

    let mut unrolled_rows = String::new();
    for member in members {
        let commit = match member.build {
            Some(build) => match build.status {
                BuildStatus::Success => {
                    let sha = &build.commit_sha;
                    format!("`{sha}`<br>([link](https://github.com/{repo}/commit/{sha}))",)
                }
                BuildStatus::Failure
                | BuildStatus::Cancelled
                | BuildStatus::Pending
                | BuildStatus::Timeouted => {
                    // This is best effort, so we ignore errors
                    let workflow_url = db
                        .get_workflow_urls_for_build(build)
                        .await
                        .unwrap_or_default()
                        .into_iter()
                        .next();
                    let status = if let Some(url) = workflow_url {
                        format!("[failed]({url})")
                    } else {
                        "failed".to_string()
                    };
                    format!(":x: build {status} :x:")
                }
            },
            None => ":x: conflicts merging into previous parent commit :x:".to_string(),
        };

        let title = format_rollup_member_message(&member.member.pr.title).replace('|', "\\|");
        writeln!(
            &mut unrolled_rows,
            "|#{pr}|{title}|{commit}|",
            pr = member.member.pr.number
        )
        .unwrap();
    }

    let truncated = parent_sha.0.chars().take(10).collect::<String>();
    let parent_sha_link = format!("[{truncated}](https://github.com/{repo}/commit/{parent_sha})");
    Comment::new(format!(
        ":pushpin: Perf builds for each rolled up PR:\n\n\
        | PR# | Message | Perf Build Sha |\n|----|----|:-----:|\n\
        {unrolled_rows}\n\
        *parent commit*: {parent_sha_link}\n\nIn the case of a perf regression, \
        run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`"
    ))
}

fn format_rollup_member_message(message: &str) -> String {
    let truncated = message.chars().take(59).collect::<String>();
    if message.chars().count() > 60 {
        format!("{truncated}…")
    } else {
        message.to_string()
    }
}

struct CompletedMember<'a> {
    member: &'a RollupMemberForUnrolling,
    build: Option<&'a BuildModel>,
}

/// Starts an unrolled build for the given rollup member and return its merge sha.
async fn start_unrolled_build(
    db: &PgDbClient,
    repo: &RepositoryState,
    rollup: &PullRequestModel,
    rollup_auto_build: &BuildModel,
    member: &RollupMemberForUnrolling,
) -> Result<CommitSha, UnrollError> {
    // The SHA upon which we will base the merge
    // This is the parent commit of the final merge SHA of the rollup
    // We fetch it from its auto build
    let base_sha = CommitSha(rollup_auto_build.parent.clone());

    // The rollup HEAD SHA that we are merging
    let head_sha = member.member.rolled_up_head_sha.clone();

    // Commit message of the merge. We lookup the intermediate member rollup merge commit from
    // GitHub.
    let message = repo
        .client
        .get_commit_message(&member.member.rolled_up_merge_sha)
        .await?;
    let Some(message) = message else {
        return Err(UnrollError::CommitNotFound {
            sha: member.member.rolled_up_merge_sha.clone(),
        });
    };

    // Note: This message is parsed by rust-timer's `triage` command
    // Note: The rollup ID is not prefixed with `#` to prevent a spam of commits referencing the rollup
    let message = format!(
        "Unrolled build for #{} in rollup {}\n{message}",
        member.pr.number, rollup.number
    );

    let res = db
        .ensure_not_concurrent(
            BuildKind::UnrolledMember,
            repo.repository(),
            async move |proof| {
                let outcome = start_build(
                    db,
                    repo,
                    &proof,
                    StartBuildContext {
                        merge_branch: TRY_PERF_MERGE_BRANCH_NAME.to_string(),
                        ci_branch: TRY_PERF_BRANCH_NAME.to_string(),
                        base_sha,
                        head_sha,
                        build_kind: BuildKind::UnrolledMember,
                    },
                    StartBuildCommit {
                        message,
                        author: bors_commit_author(),
                    },
                    // Both the members and the rollup are merged, and the GitHub UI does not show
                    // check runs for merged PRs, so this is unnecessary
                    None,
                    &member.pr,
                )
                .await
                .map_err(|e| match e {
                    StartBuildError::GithubError(e) => e,
                    StartBuildError::DatabaseError(e) => e,
                })?;
                match outcome {
                    StartBuildOutcome::Success {
                        build_commit_sha, ..
                    } => Ok(build_commit_sha),
                    StartBuildOutcome::MergeConflict => Err(UnrollError::MergeConflict),
                }
            },
        )
        .await?;
    match res {
        ExclusiveOperationOutcome::Performed(res) => res,
        ExclusiveOperationOutcome::Skipped => Err(UnrollError::Transient(anyhow::anyhow!(
            "Cannot start unrolled build due to a concurrent bors instance."
        ))),
    }
}

#[cfg(test)]
mod tests {
    use crate::database::UnrollState;
    use crate::github::{PullRequestNumber, make_rollup, rollup_state};
    use crate::tests::{
        BorsTester, Comment, MergeBehavior, PullRequest, default_repo_name, run_test,
    };
    use http::StatusCode;

    #[sqlx::test]
    async fn unroll_success(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3]).await?;

            // Ensure that waiting state has been set
            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state_all(UnrollState::Waiting);

            // Unrolling should have been triggered by the merge queue after the rollup was merged
            ctx.run_unroll_queue().await?;

            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state_all(UnrollState::Pending);

            for workflow in ctx.unrolled_workflows(2) {
                ctx.workflow_full_success(workflow).await?;
            }
            ctx.run_unroll_queue().await?;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @"
            :pushpin: Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#2|Title of PR 2|`merge-0-pr-2-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-0-pr-2-d7d45f1f-reauthored-to-bors))|
            |#3|Title of PR 3|`merge-1-pr-3-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-1-pr-3-d7d45f1f-reauthored-to-bors))|

            *parent commit*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");

            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state_all(UnrollState::Reported);
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn unroll_unrolled_build_commit_message(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;

            make_merged_rollup(ctx, &[&pr2]).await?;
            ctx.run_unroll_queue().await?;

            insta::assert_snapshot!(ctx.unrolled_branch().get_commit().message(), @"
            Unrolled build for #2 in rollup 3
            Rollup merge of #2 - default-user:pr/2, r=default-user

            Title of PR 2

            Description of PR 2
            ");

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_merge_conflict(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3]).await?;

            ctx.modify_repo(default_repo_name(), |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Cause a conflict on the second member merge
                    (n == 2).then_some(StatusCode::CONFLICT)
                }));
            });

            ctx.run_unroll_queue().await?;
            for workflow in ctx.unrolled_workflows(1) {
                ctx.workflow_full_success(workflow).await?;
            }
            ctx.run_unroll_queue().await?;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @"
            :pushpin: Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#2|Title of PR 2|`merge-0-pr-2-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-0-pr-2-d7d45f1f-reauthored-to-bors))|
            |#3|Title of PR 3|:x: conflicts merging into previous parent commit :x:|

            *parent commit*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn unroll_workflow_failure(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3]).await?;
            ctx.run_unroll_queue().await?;

            let workflows = ctx.unrolled_workflows(2);
            ctx.workflow_full_failure(workflows[0]).await?;
            ctx.workflow_full_success(workflows[1]).await?;
            ctx.run_unroll_queue().await?;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @"
            :pushpin: Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#2|Title of PR 2|:x: build [failed](https://github.com/rust-lang/borstest/actions/runs/2) :x:|
            |#3|Title of PR 3|`merge-1-pr-3-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-1-pr-3-d7d45f1f-reauthored-to-bors))|

            *parent commit*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn unroll_pr_order(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            let pr4 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;
            ctx.approve(pr4.id()).await?;
            // This PR should be the first member of the rollup, due to the priority
            ctx.post_comment(Comment::new(pr3.id(), "@bors p=5")).await?;

            let rollup = prepare_rollup(ctx, &[&pr2, &pr3, &pr4]).await?;
            // This is needed so that the rollup is actually merged first
            ctx.post_comment(Comment::new(rollup, "@bors p=10")).await?;
            ctx.start_and_finish_auto_build(rollup).await?;

            ctx.run_unroll_queue().await?;

            for workflow in ctx.unrolled_workflows(3) {
                ctx.workflow_full_success(workflow).await?;
            }
            ctx.run_unroll_queue().await?;

            let comment = ctx.get_next_comment_text(rollup).await?;
            insta::assert_snapshot!(comment, @"
            :pushpin: Perf builds for each rolled up PR:

            | PR# | Message | Perf Build Sha |
            |----|----|:-----:|
            |#3|Title of PR 3|`merge-1-pr-3-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-1-pr-3-d7d45f1f-reauthored-to-bors))|
            |#2|Title of PR 2|`merge-0-pr-2-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-0-pr-2-d7d45f1f-reauthored-to-bors))|
            |#4|Title of PR 4|`merge-2-pr-4-d7d45f1f-reauthored-to-bors`<br>([link](https://github.com/rust-lang/borstest/commit/merge-2-pr-4-d7d45f1f-reauthored-to-bors))|

            *parent commit*: [main-sha1](https://github.com/rust-lang/borstest/commit/main-sha1)

            In the case of a perf regression, run the following command for each PR you suspect might be the cause: `@rust-timer build $SHA`
            ");
            Ok(())
        })
            .await;
    }

    #[sqlx::test]
    async fn unroll_recover_transient_error(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3]).await?;

            ctx.modify_repo(default_repo_name(), |repo| {
                repo.merge_behavior =
                    MergeBehavior::Custom(Box::new(|| Some(StatusCode::INTERNAL_SERVER_ERROR)));
            });

            // Transient error during merging
            assert!(ctx.run_unroll_queue().await.is_err());
            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state_all(UnrollState::Waiting);

            ctx.modify_repo(default_repo_name(), |repo| {
                repo.merge_behavior = MergeBehavior::default();
            });

            ctx.trigger_and_run_unroll_queue().await?;
            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state_all(UnrollState::Pending);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_recover_handle_all_members(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            let rollup = make_merged_rollup(ctx, &[&pr2, &pr3]).await?;

            // Ensure that if one of the members has an error, we will continue processing
            // the other ones.
            ctx.modify_repo(default_repo_name(), |repo| {
                let mut n = 0;
                repo.merge_behavior = MergeBehavior::Custom(Box::new(move || {
                    n += 1;
                    // Cause a conflict only on the first member merge
                    (n == 1).then_some(StatusCode::CONFLICT)
                }));
            });

            // Run the unrolling once, the second member should still be processed
            // even if processing the first member fails
            ctx.run_unroll_queue().await?;
            ctx.get_rollup(rollup)
                .await?
                .expect_unroll_state(2, UnrollState::Finished)
                .expect_unroll_state(3, UnrollState::Pending);

            Ok(())
        })
        .await;
    }

    #[sqlx::test]
    async fn unroll_concurrent_bors_instances(pool: sqlx::PgPool) {
        run_test((pool, rollup_state()), async |ctx: &mut BorsTester| {
            let pr2 = ctx.open_pr((), |_| {}).await?;
            let pr3 = ctx.open_pr((), |_| {}).await?;
            ctx.approve(pr2.id()).await?;
            ctx.approve(pr3.id()).await?;

            make_merged_rollup(ctx, &[&pr2, &pr3]).await?;

            let concurrent_futs = (0..10)
                .map(|_| async { ctx.run_unroll_queue_concurrent().await })
                .collect::<Vec<_>>();
            futures::future::join_all(concurrent_futs).await;

            let branch = ctx.unrolled_branch();
            assert_eq!(branch.get_commit_history().len(), 2);

            Ok(())
        })
        .await;
    }

    /// Creates and approves a rollup and waits until it is merged.
    async fn make_merged_rollup(
        ctx: &mut BorsTester,
        prs: &[&PullRequest],
    ) -> anyhow::Result<PullRequestNumber> {
        let pr_number = prepare_rollup(ctx, prs).await?;
        ctx.start_and_finish_auto_build(pr_number).await?;
        Ok(pr_number)
    }

    /// Creates and approves a rollup.
    async fn prepare_rollup(
        ctx: &mut BorsTester,
        prs: &[&PullRequest],
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
        Ok(pr_number)
    }
}
