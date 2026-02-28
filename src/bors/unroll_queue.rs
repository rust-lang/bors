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

