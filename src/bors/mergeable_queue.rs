use tokio::sync::mpsc;

use crate::github::{GithubRepoName, PullRequestNumber};

#[derive(Debug)]
pub struct MergeableQueueItem {
    pub repository: GithubRepoName,
    pub pr_number: PullRequestNumber,
}

pub struct MergeableQueue {
    tx: Option<mpsc::Sender<MergeableQueueItem>>,
}

impl MergeableQueue {
    pub fn new() -> Self {
        Self { tx: None }
    }

    pub fn with_sender(tx: mpsc::Sender<MergeableQueueItem>) -> Self {
        Self { tx: Some(tx) }
    }

    pub async fn push(&self, repository: GithubRepoName, pr_number: PullRequestNumber) {
        if let Some(tx) = &self.tx {
            let item = MergeableQueueItem {
                repository,
                pr_number,
            };
            if let Err(e) = tx.send(item).await {
                tracing::error!("Failed to push to mergeable queue: {}", e);
            }
        }
    }
}
