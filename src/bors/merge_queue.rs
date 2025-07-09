use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BorsContext;
use crate::bors::RepositoryState;
use crate::utils::sort_queue::sort_queue_prs;

type MergeQueueEvent = ();
pub type MergeQueueSender = mpsc::Sender<MergeQueueEvent>;

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

    let fut = async move {
        while rx.recv().await.is_some() {
            let span = tracing::info_span!("MergeQueue");
            tracing::debug!("Processing merge queue");
            if let Err(error) = merge_queue_tick(ctx.clone()).instrument(span.clone()).await {
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
    };

    (tx, fut)
}
