use crate::BorsContext;
use crate::bors::approval::resolve_tentative_approval;
use crate::bors::handlers::PullRequestData;
use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::{PullRequestStatus, RepositoryState};
use crate::database::{ApprovalInfo, PullRequestModel};
use crate::github::{GithubRepoName, PullRequest};
use std::sync::Arc;
use tokio::sync::mpsc;

pub type ApprovalQueueReceiver = mpsc::Receiver<ApprovalQueueEvent>;

#[derive(Clone)]
pub struct ApprovalQueueSender {
    inner: mpsc::Sender<ApprovalQueueEvent>,
}

impl ApprovalQueueSender {
    pub async fn refresh_tentative_approvals(
        &self,
        repository: GithubRepoName,
    ) -> Result<(), mpsc::error::SendError<ApprovalQueueEvent>> {
        self.inner
            .send(ApprovalQueueEvent::RefreshTentativeApprovals(repository))
            .await
    }
}

pub fn create_approval_queue() -> (ApprovalQueueSender, ApprovalQueueReceiver) {
    let (tx, rx) = mpsc::channel(1024);
    (ApprovalQueueSender { inner: tx }, rx)
}

#[derive(Debug)]
pub enum ApprovalQueueEvent {
    RefreshTentativeApprovals(GithubRepoName),
}

pub async fn handle_approval_queue_event(
    ctx: Arc<BorsContext>,
    event: ApprovalQueueEvent,
    merge_queue_tx: MergeQueueSender,
) -> anyhow::Result<()> {
    match event {
        ApprovalQueueEvent::RefreshTentativeApprovals(repository) => {
            let repo = ctx.get_repo(&repository)?;
            let pull_requests = ctx.db.get_nonclosed_pull_requests(&repository).await?;

            for pr in pull_requests {
                if let Err(error) =
                    process_tentative_approval(&ctx, &repo, &pr, &merge_queue_tx).await
                {
                    tracing::error!(
                        "Failed to process tentative approval for PR #{}: {error:?}",
                        pr.number
                    );
                }
            }
        }
    }

    Ok(())
}

#[derive(Debug)]
enum SanityCheckError {
    /// The pull request was not open.
    WrongStatus,
    /// The pull request head SHA was different from the tentatively approved SHA.
    ApprovedShaMismatch,
}

fn sanity_check_pr(
    gh_pr: &PullRequest,
    approval_info: &ApprovalInfo,
) -> Result<(), SanityCheckError> {
    if gh_pr.status != PullRequestStatus::Open {
        return Err(SanityCheckError::WrongStatus);
    }

    if gh_pr.head.sha.as_ref() != approval_info.sha {
        return Err(SanityCheckError::ApprovedShaMismatch);
    }

    Ok(())
}

async fn process_tentative_approval(
    ctx: &BorsContext,
    repo: &RepositoryState,
    pr: &PullRequestModel,
    merge_queue_tx: &MergeQueueSender,
) -> anyhow::Result<()> {
    let Some(approval_info) = pr.tentative_approval() else {
        return Ok(());
    };

    let pr_number = pr.number;
    let gh_pr = repo.client.get_pull_request(pr_number).await?;

    if let Err(error) = sanity_check_pr(&gh_pr, approval_info) {
        tracing::info!(
            "Removing tentative approval for PR #{pr_number} after sanity check failed: {error:?}"
        );
        ctx.db.remove_tentative_approval(pr).await?;
        return Ok(());
    }

    resolve_tentative_approval(
        ctx,
        repo,
        PullRequestData {
            github: &gh_pr,
            db: pr,
        },
        &approval_info.approver,
        Vec::new(),
        pr.priority.map(|priority| priority as u32),
        merge_queue_tx,
    )
    .await?;
    Ok(())
}
