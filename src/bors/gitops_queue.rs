//! The gitops queue is a background process that performs expensive git operations serially.
//! It is used for operations that cannot be performed using the GitHub API and have to be done
//! using local git operations (currently through libgit2).

use crate::github::{CommitSha, GithubRepoName};
use anyhow::Context;
use secrecy::SecretString;
use std::fmt::{Debug, Formatter};
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::Instrument;

/// Maximum number of pending git operations in the queue.
/// If the queue is full, new operations will be rejected with an error message.
const GITOPS_QUEUE_CAPACITY: usize = 3;

/// Maximum duration of a local git operation before it times out.
const GITOP_TIMEOUT: Duration = Duration::from_secs(60);

pub type GitOpsQueueReceiver = mpsc::Receiver<GitOpsCommand>;

#[derive(Clone)]
pub struct GitOpsQueueSender {
    inner: mpsc::Sender<GitOpsCommand>,
}

impl GitOpsQueueSender {
    /// Try to enqueue a git operation.
    /// Returns `true` if the operation was enqueued or `false` if the queue is full.
    pub fn try_send(&self, command: GitOpsCommand) -> anyhow::Result<bool> {
        match self.inner.try_send(command) {
            Ok(_) => Ok(true),
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

pub fn create_gitops_queue() -> (GitOpsQueueSender, GitOpsQueueReceiver) {
    let (tx, rx) = mpsc::channel(GITOPS_QUEUE_CAPACITY);
    (GitOpsQueueSender { inner: tx }, rx)
}

pub async fn handle_gitops_command(command: GitOpsCommand) -> anyhow::Result<()> {
    match command {
        GitOpsCommand::Push(PushCommand {
            source_repo,
            target_repo,
            target_branch,
            commit,
            token,
            on_finish,
        }) => {
            let span = tracing::debug_span!(
                "push commit to repository",
                "{source_repo}:{commit} -> {target_repo}:{target_branch}"
            );
            let res = {
                let fut = tokio::task::spawn_blocking(move || {
                    use std::time::Instant;

                    let start = Instant::now();
                    let res =
                        execute_push(&source_repo, &target_repo, &commit, &target_branch, token);
                    tracing::trace!("Push took {}", start.elapsed().as_secs_f64());
                    res
                })
                .instrument(span.clone());
                match tokio::time::timeout(GITOP_TIMEOUT, fut).await {
                    Ok(res) => res.map_err(|e| anyhow::anyhow!("{e}"))?,
                    Err(_) => Err(anyhow::anyhow!("Push timeouted")),
                }
            };

            if let Err(error) = on_finish(res).instrument(span.clone()).await {
                span.in_scope(|| {
                    tracing::error!("Completion callback failed: {error:?}");
                });

                #[cfg(test)]
                return Err(error);
            }
        }
    }
    Ok(())
}

#[cfg_attr(test, allow(unreachable_code, unused))]
fn execute_push(
    source_repo: &GithubRepoName,
    target_repo: &GithubRepoName,
    commit: &CommitSha,
    target_branch: &str,
    token: SecretString,
) -> anyhow::Result<()> {
    #[cfg(test)]
    return Ok(());

    use git2::{Cred, FetchOptions, PushOptions, RemoteCallbacks, Repository};
    use secrecy::ExposeSecret;

    let source_repo_url = format!("https://github.com/{source_repo}.git");
    let target_repo_url = format!("https://github.com/{target_repo}.git");
    let target_branch = format!("refs/heads/{target_branch}");

    // Create a temporary directory for the local repository
    let temp_dir = tempfile::tempdir()?;
    let repo_path = temp_dir.path();
    let repo = Repository::init_bare(repo_path)?;

    let callbacks = || {
        let token = token.clone();
        let mut callbacks = RemoteCallbacks::new();
        callbacks.credentials(move |_url, _username_from_url, _allowed_types| {
            Cred::userpass_plaintext("bors", token.expose_secret())
        });
        callbacks
    };

    let mut source_remote = repo.remote_anonymous(&source_repo_url)?;
    let mut fetch_options = FetchOptions::new();
    // Only fetch the single commit
    fetch_options.depth(1);
    fetch_options.remote_callbacks(callbacks());

    tracing::debug!("Fetching commit");
    // Fetch the commit from the source repo
    source_remote
        .fetch(&[commit.as_ref()], Some(&mut fetch_options), None)
        .with_context(|| anyhow::anyhow!("Cannot fetch commit {commit} from {source_repo}"))?;

    let oid = git2::Oid::from_str(commit.as_ref())?;
    if let Err(error) = repo.find_commit(oid) {
        return Err(anyhow::anyhow!(
            "Cannot find commit {commit} from {source_repo}: {error:?}"
        ));
    }

    // Create the refspec: push the commit to the target branch
    // The `+` sign says that it is a force push
    let refspec = format!("+{commit}:{target_branch}");

    tracing::debug!("Pushing commit");
    // And push it to the fork repo
    let mut target_remote = repo.remote_anonymous(&target_repo_url)?;
    let mut push_options = PushOptions::new();
    push_options.remote_callbacks(callbacks());

    target_remote
        .push(&[&refspec], Some(&mut push_options))
        .with_context(|| anyhow::anyhow!("Cannot push commit {commit} to {target_repo}"))?;
    Ok(())
}
