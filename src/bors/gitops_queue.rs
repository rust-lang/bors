//! The gitops queue is a background process that performs expensive git operations serially.
//! It is used for operations that cannot be performed using the GitHub API and have to be done
//! using local git operations (currently through libgit2).

use crate::bors::gitops::Git;
use crate::github::{CommitSha, GithubRepoName, PullRequestNumber};
use secrecy::SecretString;
use std::collections::HashSet;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::Instrument;

/// Maximum number of pending git operations in the queue.
/// If the queue is full, new operations will be rejected with an error message.
const GITOPS_QUEUE_CAPACITY: usize = 3;

/// Maximum duration of a local git operation before it times out.
const GITOP_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct PullRequestId {
    pub repo: GithubRepoName,
    pub pr: PullRequestNumber,
}

#[derive(Debug)]
pub struct GitOpsQueueEntry {
    command: GitOpsCommand,
    pr: PullRequestId,
}

struct GitOpsSharedState {
    git: Option<Git>,
    /// Pull requests on which a local git operation is currently queued or in-progress.
    pending_prs: HashSet<PullRequestId>,
}

pub struct GitOpsQueueReceiver {
    receiver: mpsc::Receiver<GitOpsQueueEntry>,
    state: Arc<RwLock<GitOpsSharedState>>,
}

impl GitOpsQueueReceiver {
    pub async fn recv(&mut self) -> Option<GitOpsQueueEntry> {
        self.receiver.recv().await
    }
}

#[derive(Clone)]
pub struct GitOpsQueueSender {
    sender: mpsc::Sender<GitOpsQueueEntry>,
    state: Arc<RwLock<GitOpsSharedState>>,
}

impl GitOpsQueueSender {
    /// Returns `true` if the given pull request already has a queued git operation.
    pub fn is_pending(&self, id: &PullRequestId) -> bool {
        self.state.read().unwrap().pending_prs.contains(id)
    }

    /// Try to enqueue a git operation.
    /// Returns `true` if the operation was enqueued or `false` if the queue is full.
    pub fn try_send(&self, id: PullRequestId, command: GitOpsCommand) -> anyhow::Result<bool> {
        let entry = GitOpsQueueEntry {
            command,
            pr: id.clone(),
        };
        match self.sender.try_send(entry) {
            Ok(_) => {
                self.state.write().unwrap().pending_prs.insert(id);
                Ok(true)
            }
            Err(mpsc::error::TrySendError::Full(_)) => Ok(false),
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(anyhow::anyhow!("Gitops queue was closed"))
            }
        }
    }
}

/// Command that can be executed by the gitops queue.
#[derive(Debug)]
pub enum GitOpsCommand {
    /// Push a commit from one repository to another.
    Push(PushCommand),
}

pub type PushCallback = Box<
    dyn FnOnce(anyhow::Result<()>) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>
        + Send,
>;

/// Force push `commit` from `source_repo` to `target_branch` of `target_repo`.
/// Use `token` for authentication.
///
/// After the push finishes, perform `on_finish` and pass it the result of the push operation.
pub struct PushCommand {
    pub source_repo: GithubRepoName,
    pub target_repo: GithubRepoName,
    pub target_branch: String,
    pub commit: CommitSha,
    pub token: SecretString,
    pub on_finish: PushCallback,
}

impl Debug for PushCommand {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let Self {
            source_repo,
            target_repo,
            target_branch,
            commit,
            token: _,
            on_finish: _,
        } = self;
        f.debug_struct("PushCommand")
            .field("source_repo", source_repo)
            .field("target_repo", target_repo)
            .field("target_branch", target_branch)
            .field("commit", commit)
            .finish()
    }
}

pub fn create_gitops_queue(git: Option<Git>) -> (GitOpsQueueSender, GitOpsQueueReceiver) {
    let (tx, rx) = mpsc::channel(GITOPS_QUEUE_CAPACITY);
    let state = Arc::new(RwLock::new(GitOpsSharedState {
        pending_prs: Default::default(),
        git,
    }));
    (
        GitOpsQueueSender {
            sender: tx,
            state: state.clone(),
        },
        GitOpsQueueReceiver {
            receiver: rx,
            state,
        },
    )
}

pub async fn handle_gitops_entry(
    rx: &GitOpsQueueReceiver,
    entry: GitOpsQueueEntry,
) -> anyhow::Result<()> {
    let GitOpsQueueEntry { command, pr } = entry;
    let handle = async move {
        match command {
            GitOpsCommand::Push(PushCommand {
                source_repo,
                target_repo,
                target_branch,
                commit,
                token: _token,
                on_finish,
            }) => {
                let span = tracing::debug_span!(
                    "push commit to repository",
                    "{source_repo}:{commit} -> {target_repo}:{target_branch}"
                );

                let git = rx.state.read().unwrap().git.clone();
                let res = if let Some(_git) = git {
                    let fut = async move {
                        use std::time::Instant;

                        let start = Instant::now();
                        #[cfg(test)]
                        let res = anyhow::Ok(());
                        #[cfg(not(test))]
                        let res = _git
                            .transfer_commit_between_repositories(
                                &source_repo,
                                &target_repo,
                                &commit,
                                &target_branch,
                                _token,
                            )
                            .await;
                        tracing::trace!("Push took {:.3}s", start.elapsed().as_secs_f64());
                        res
                    }
                    .instrument(span.clone());
                    match tokio::time::timeout(GITOP_TIMEOUT, fut).await {
                        Ok(res) => res,
                        Err(_) => Err(anyhow::anyhow!("Push timeouted")),
                    }
                } else {
                    Err(anyhow::anyhow!("Local git is not available"))
                };

                if let Err(error) = on_finish(res).instrument(span.clone()).await {
                    span.in_scope(|| {
                        tracing::error!("Completion callback failed: {error:?}");
                    });

                    #[cfg(test)]
                    return Err(error);
                }
                Ok(())
            }
        }
    };

    let res = handle.await;
    rx.state.write().unwrap().pending_prs.remove(&pr);
    res
}
