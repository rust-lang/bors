use anyhow::anyhow;
use octocrab::models::StatusState;

use std::sync::Arc;

use crate::{
    BorsContext,
    bors::{
        PullRequestStatus, RepositoryState,
        comment::{
            auto_build_push_failed_comment, auto_build_started_comment, merge_conflict_comment,
        },
        handlers::labels::handle_label_trigger,
    },
    database::{BuildStatus, MergeableState, PullRequestModel},
    github::{CommitSha, LabelTrigger, MergeError, api::client::GithubRepositoryClient},
    utils::sort_queue::sort_queue_prs,
};

/// Branch used for performing merge operations.
/// This branch should not run CI checks.
pub(super) const AUTO_MERGE_BRANCH_NAME: &str = "automation/bors/auto-merge";

/// Branch where CI checks run for merge builds.
/// This branch should run CI checks.
pub(super) const AUTO_BRANCH_NAME: &str = "automation/bors/auto";

pub type MergeQueueEvent = ();

enum MergeResult {
    Success(CommitSha),
    Conflict,
}

pub async fn handle_merge_queue(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();

    for repo in repos {
        let repo_name = repo.repository();
        let repo_db = match ctx.db.repo_db(repo_name).await? {
            Some(repo) => repo,
            None => {
                tracing::error!("Repository {} not found", repo_name);
                continue;
            }
        };
        let priority = repo_db.tree_state.priority();
        let prs = ctx.db.get_merge_queue_prs(repo_name, priority).await?;

        // Sort PRs according to merge queue priority rules.
        // Pending PRs come first - this is important as we make sure to block the queue to
        // prevent starting simultaneous auto-builds.
        let prs = sort_queue_prs(prs);

        for pr in prs {
            let pr_num = pr.number;

            if let Some(auto_build) = &pr.auto_build {
                let commit_sha = CommitSha(auto_build.commit_sha.clone());

                match auto_build.status {
                    // Build in progress - stop queue. We can only have one PR built at a time.
                    BuildStatus::Pending => {
                        tracing::debug!(
                            "PR {repo_name}/{pr_num} has a pending build - blocking queue"
                        );
                        break;
                    }
                    // Build successful - point the base branch to the merged commit.
                    BuildStatus::Success => {
                        match repo
                            .client
                            .set_branch_to_sha(&pr.base_branch, &commit_sha)
                            .await
                        {
                            Ok(()) => {
                                tracing::info!("Auto build succeeded and merged");
                                ctx.db
                                    .set_pr_status(
                                        &pr.repository,
                                        pr.number,
                                        PullRequestStatus::Merged,
                                    )
                                    .await?;
                            }
                            Err(e) => {
                                tracing::error!("Failed to push to base branch: {:?}", e);

                                let comment =
                                    auto_build_push_failed_comment(&commit_sha, &pr.base_branch);
                                repo.client.post_comment(pr.number, comment).await?;

                                // Continue to the next PR and keep it in the queue.
                                continue;
                            }
                        };
                    }
                    BuildStatus::Failure | BuildStatus::Cancelled | BuildStatus::Timeouted => {
                        unreachable!("Failed auto builds should be filtered out by SQL query");
                    }
                }
            }

            // No build exists for this PR - try to start a new merge build.
            match start_auto_build(&repo, &ctx, pr).await {
                Ok(true) => {
                    tracing::info!("Starting merge build for PR {pr_num}");
                    // We can only have one PR being built at a time - block the queue.
                    break;
                }
                Ok(false) => {
                    // Failed due to issue with the PR (e.g. merge conflicts).
                    tracing::debug!("Failed to start merge build for PR {pr_num}");
                    continue;
                }
                Err(error) => {
                    // Unexpected error - the PR will remain in the "queue" for a retry.
                    tracing::error!("Error starting merge build for PR {pr_num}: {:?}", error);
                    continue;
                }
            }
        }
    }

    Ok(())
}

/// Starts a new auto build for a pull request.
async fn start_auto_build(
    repo: &Arc<RepositoryState>,
    ctx: &Arc<BorsContext>,
    pr: PullRequestModel,
) -> anyhow::Result<bool> {
    let client = &repo.client;

    let gh_pr = client.get_pull_request(pr.number).await?;
    let base_sha = client.get_branch_sha(&pr.base_branch).await?;

    let auto_merge_commit_message = format!(
        "Auto merge of #{} - {}, r={}\n\n{}\n\n{}",
        pr.number,
        gh_pr.head_label,
        pr.approver().unwrap_or("<unknown>"),
        pr.title,
        gh_pr.message
    );

    // 1. Merge PR head with base branch on `AUTO_MERGE_BRANCH_NAME`
    match attempt_merge(
        &repo.client,
        &gh_pr.head.sha,
        &base_sha,
        &auto_merge_commit_message,
    )
    .await?
    {
        MergeResult::Success(merge_sha) => {
            // 2. Push merge commit to `AUTO_BRANCH_NAME` where CI runs
            client
                .set_branch_to_sha(AUTO_BRANCH_NAME, &merge_sha)
                .await?;

            // 3. Record the build in the database
            ctx.db
                .attach_auto_build(
                    &pr,
                    AUTO_BRANCH_NAME.to_string(),
                    merge_sha.clone(),
                    base_sha,
                )
                .await?;

            // 4. Update label
            handle_label_trigger(repo, pr.number, LabelTrigger::AutoBuildStarted).await?;

            // 5. Post status comment
            let comment = auto_build_started_comment(&gh_pr.head.sha, &merge_sha);
            client.post_comment(pr.number, comment).await?;

            // 6. Set GitHub commit status to pending on PR head
            let desc = format!(
                "Testing commit {} with merge {}...",
                gh_pr.head.sha, merge_sha
            );
            client
                .create_commit_status(
                    &gh_pr.head.sha,
                    StatusState::Pending,
                    None,
                    Some(&desc),
                    Some("bors"),
                )
                .await?;

            Ok(true)
        }
        MergeResult::Conflict => {
            repo.client
                .post_comment(pr.number, merge_conflict_comment(&gh_pr.head.name))
                .await?;
            ctx.db
                .update_pr_mergeable_state(&pr, MergeableState::HasConflicts)
                .await?;

            Ok(false)
        }
    }
}

/// Attempts to merge the given head SHA with base SHA via `AUTO_MERGE_BRANCH_NAME`.
async fn attempt_merge(
    client: &GithubRepositoryClient,
    head_sha: &CommitSha,
    base_sha: &CommitSha,
    merge_message: &str,
) -> anyhow::Result<MergeResult> {
    tracing::debug!("Attempting to merge with base SHA {base_sha}");

    // Reset auto merge branch to point to base branch
    client
        .set_branch_to_sha(AUTO_MERGE_BRANCH_NAME, base_sha)
        .await
        .map_err(|error| anyhow!("Cannot set auto merge branch to {}: {error:?}", base_sha.0))?;

    // then merge PR head commit into auto merge branch.
    match client
        .merge_branches(AUTO_MERGE_BRANCH_NAME, head_sha, merge_message)
        .await
    {
        Ok(merge_sha) => {
            tracing::debug!("Merge successful, SHA: {merge_sha}");

            Ok(MergeResult::Success(merge_sha))
        }
        Err(MergeError::Conflict) => {
            tracing::warn!("Merge conflict");

            Ok(MergeResult::Conflict)
        }
        Err(error) => Err(error.into()),
    }
}
