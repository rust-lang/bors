use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BorsContext;
use crate::bors::RepositoryState;
use crate::utils::sort_queue::sort_queue_prs;

#[derive(Debug)]
enum MergeQueueEvent {
    Trigger,
    Shutdown,
}

#[derive(Clone)]
pub struct MergeQueueSender {
    inner: mpsc::Sender<MergeQueueEvent>,
}

impl MergeQueueSender {
    pub async fn trigger(&self) -> Result<(), mpsc::error::SendError<()>> {
        self.inner
            .send(MergeQueueEvent::Trigger)
            .await
            .map_err(|_| mpsc::error::SendError(()))
    }

    pub fn shutdown(&self) {
        let _ = self.inner.try_send(MergeQueueEvent::Shutdown);
    }
}

/// Branch where CI checks run for auto builds.
/// This branch should run CI checks.
pub(super) const AUTO_BRANCH_NAME: &str = "automation/bors/auto";

pub async fn merge_queue_tick(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();

    for repo in repos {
        let repo_name = repo.repository();
        let repo_db = match ctx.db.repo_db(repo_name).await? {
            Some(repo) => repo,
            None => {
                tracing::error!("Repository {repo_name} not found");
                continue;
            }
        };

        if !repo.config.load().merge_queue_enabled {
            continue;
        }

        let priority = repo_db.tree_state.priority();
        let prs = ctx.db.get_merge_queue_prs(repo_name, priority).await?;

        // Sort PRs according to merge queue priority rules.
        // Successful builds come first so they can be merged immediately,
        // then pending builds (which block the queue to prevent starting simultaneous auto-builds).
        let prs = sort_queue_prs(prs);

        for _ in prs {
            // Process PRs...
        }
    }

    Ok(())
}

pub fn start_merge_queue(ctx: Arc<BorsContext>) -> (MergeQueueSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<MergeQueueEvent>(10);
    let sender = MergeQueueSender { inner: tx };

    let fut = async move {
        while let Some(event) = rx.recv().await {
            match event {
                MergeQueueEvent::Trigger => {
                    let span = tracing::info_span!("MergeQueue");
                    tracing::debug!("Processing merge queue");
                    if let Err(error) = merge_queue_tick(ctx.clone()).instrument(span.clone()).await
                    {
                        // In tests, we want to panic on all errors.
                        #[cfg(test)]
                        {
                            panic!("Merge queue handler failed: {error:?}");
                        }
                        #[cfg(not(test))]
                        {
                            use crate::utils::logging::LogError;
                            span.log_error(error);
                        }
                    }
                }
                MergeQueueEvent::Shutdown => {
                    tracing::debug!("Merge queue received shutdown signal");
                    break;
                }
            }
        }
    };

    (sender, fut)
}
