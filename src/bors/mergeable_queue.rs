//! The mergeability queue serves for background fetching of pull requests from the GitHub API,
//! to determine their mergeability status.
//!
//! The mergeability status cannot be determined directly from GitHub, because it is computed by a
//! background job (see https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-your-git-database).
//!
//! You can add a PR to the mergeability queue. The queue will then immediately contact the GH API
//! (by GETing the PR from GitHub), which should start the background job. After that, the PR will
//! be checked in increasing intervals (after 5s, then after 10s, then after 15s, etc.), until we
//! either get a known mergeability status from GH or until we run out of retries.

use super::BorsContext;
use crate::database::OctocrabMergeableState;
use crate::github::{GithubRepoName, PullRequestNumber};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::time::timeout;

/// Base delay before two mergeability check attempts.
#[cfg(not(test))]
const BASE_DELAY: Duration = Duration::from_secs(5);

#[cfg(test)]
const BASE_DELAY: Duration = Duration::from_millis(500);

/// Max number of mergeable check retries before giving up.
const MAX_RETRIES: u32 = 5;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct QueuedPullRequest {
    pub pr_number: PullRequestNumber,
    pub repo: GithubRepoName,
}

impl std::fmt::Display for QueuedPullRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/#{}", self.repo, self.pr_number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct MergeableQueueItem {
    pub pull_request: QueuedPullRequest,
    pub attempt: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum QueueMessage {
    Shutdown,
    Item(MergeableQueueItem),
}

#[derive(PartialEq, Eq)]
struct Item {
    /// When to process item (None = immediate).
    /// Reversed to create min-heap for expirations.
    expiration: Reverse<Option<Instant>>,
    inner: QueueMessage,
}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expiration
            .cmp(&other.expiration)
            .then_with(|| self.inner.cmp(&other.inner))
    }
}

struct SharedInner {
    queue: Mutex<BinaryHeap<Item>>,
    notify: Notify,
}

#[derive(Clone)]
pub struct MergeableQueueSender {
    inner: Arc<SharedInner>,
}

pub struct MergeableQueueReceiver {
    inner: Arc<SharedInner>,
}

pub fn create_mergeable_queue() -> (MergeableQueueSender, MergeableQueueReceiver) {
    let shared = Arc::new(SharedInner {
        queue: Mutex::new(BinaryHeap::new()),
        notify: Notify::new(),
    });

    (
        MergeableQueueSender {
            inner: shared.clone(),
        },
        MergeableQueueReceiver { inner: shared },
    )
}

impl MergeableQueueSender {
    /// Shutdown the mergeability queue.
    pub fn shutdown(&self) {
        let mut queue = self.inner.queue.lock().unwrap();

        // Send shutdown message
        queue.push(Item {
            expiration: Reverse(None),
            inner: QueueMessage::Shutdown,
        });

        // and wake receiver for immediate processing.
        self.inner.notify.notify_one();
    }

    /// Enqueues the given PR for a mergeability check.
    /// It will be repeatedly fetched in the background from the GH API, until we can figure out
    /// its mergeability status.
    pub fn enqueue_pr(&self, repo: GithubRepoName, pr_number: PullRequestNumber) {
        self.insert_item(
            MergeableQueueItem {
                pull_request: QueuedPullRequest { pr_number, repo },
                attempt: 1,
            },
            None,
        );
    }

    fn enqueue_retry(&self, queue_item: MergeableQueueItem) {
        // First attempt = BASE_DELAY
        // Second attempt = BASE_DELAY * 2
        // Third attempt = BASE_DELAY * 3
        // etc.
        let delay = BASE_DELAY * queue_item.attempt;
        let expiration = Some(Instant::now() + delay);
        let next_attempt = queue_item.attempt + 1;

        self.insert_item(
            MergeableQueueItem {
                pull_request: queue_item.pull_request,
                attempt: next_attempt,
            },
            expiration,
        );
    }

    fn insert_item(&self, item: MergeableQueueItem, expiration: Option<Instant>) {
        let mut queue = self.inner.queue.lock().unwrap();

        // Notify when:
        // 1. The current item expires sooner than the head of the queue
        let has_earlier_expiration =
            queue
                .peek()
                .is_some_and(|head| match (expiration, head.expiration) {
                    (Some(new_exp), Reverse(Some(head_exp))) => new_exp < head_exp,
                    _ => false,
                });
        // 2. The queue was empty before insertion (reader might be waiting)
        let should_notify = queue.is_empty() || expiration.is_none() || has_earlier_expiration;

        queue.push(Item {
            expiration: Reverse(expiration),
            inner: QueueMessage::Item(item),
        });

        if should_notify {
            self.inner.notify.notify_one();
        }
    }
}

impl MergeableQueueReceiver {
    /// Get the next item from the queue.
    pub async fn dequeue(&self) -> Option<(MergeableQueueItem, MergeableQueueSender)> {
        loop {
            match self.peek_inner() {
                // Item is ready.
                Ok(QueueMessage::Item(item)) => {
                    break Some((
                        item,
                        MergeableQueueSender {
                            inner: self.inner.clone(),
                        },
                    ));
                }
                Ok(QueueMessage::Shutdown) => {
                    break None;
                }
                // Item exists but is not ready, wait until then or until notified of a higher
                // priority item.
                Err(Some(duration)) => {
                    let _ = timeout(duration, self.inner.notify.notified()).await;
                }
                // Queue is empty, wait until notified of a new item.
                Err(None) => {
                    self.inner.notify.notified().await;
                }
            }
        }
    }

    /// Try to remove an item from the mergeability queue that should be processed next.
    /// If no item should be processed at this time, returns an optional duration before the next
    /// item should be available.
    /// This duration is not known if the queue is empty.
    fn peek_inner(&self) -> Result<QueueMessage, Option<Duration>> {
        let now = Instant::now();
        let mut queue = self.inner.queue.lock().unwrap();

        match queue.peek() {
            // Immediate item, ready for processing.
            Some(Item {
                expiration: Reverse(None),
                ..
            }) => {
                let item = queue.pop().unwrap().inner;
                Ok(item)
            }
            // Expiration has passed, ready for processing.
            Some(Item {
                expiration: Reverse(Some(expiration)),
                ..
            }) if *expiration <= now => {
                let item = queue.pop().unwrap().inner;
                Ok(item)
            }
            // Scheduled for future, wait until it's ready.
            Some(Item {
                expiration: Reverse(Some(expiration)),
                ..
            }) => {
                let wait_time = *expiration - now;
                Err(Some(wait_time))
            }
            // Empty queue, wait for an item to be added.
            None => Err(None),
        }
    }
}

pub async fn handle_mergeable_queue_item(
    ctx: Arc<BorsContext>,
    mq_tx: MergeableQueueSender,
    mq_item: MergeableQueueItem,
) -> anyhow::Result<()> {
    let MergeableQueueItem {
        ref pull_request,
        ref attempt,
    } = mq_item;

    if *attempt >= MAX_RETRIES {
        tracing::warn!("Exceeded max mergeable state attempts for PR: {pull_request}");
        return Ok(());
    }

    let repo_state = match ctx.repositories.read() {
        Ok(guard) => match guard.get(&pull_request.repo) {
            Some(state) => state.clone(),
            None => {
                return Err(anyhow::anyhow!(
                    "Repository not found: {}",
                    pull_request.repo
                ));
            }
        },
        Err(err) => {
            return Err(anyhow::anyhow!(
                "Failed to acquire read lock on repositories: {err:?}",
            ));
        }
    };

    // Load the PR from GitHub.
    // - If the PR's mergeability is unknown, and the GH background job hasn't been started yet,
    //   this PR fetch will trigger its start.
    // - If the PR mergeability is known, we will be able to read it and update it in the DB.
    let fetched_pr = repo_state
        .client
        .get_pull_request(pull_request.pr_number)
        .await?;
    let new_mergeable_state = fetched_pr.mergeable_state;

    // We don't know the mergeability state yet. Retry the PR after some delay
    if new_mergeable_state == OctocrabMergeableState::Unknown {
        tracing::info!("Mergeability status unknown, scheduling retry.");

        mq_tx.enqueue_retry(mq_item);

        return Ok(());
    } else {
        tracing::info!("Received mergeability status {new_mergeable_state:?}");
    }

    // We know the mergeability status, so update it in the DB
    let pr_model = match ctx
        .db
        .get_pull_request(&pull_request.repo, pull_request.pr_number)
        .await?
    {
        Some(model) => model,
        None => {
            return Err(anyhow::anyhow!("PR not found in database: {pull_request}"));
        }
    };

    ctx.db
        .update_pr_mergeable_state(&pr_model, new_mergeable_state.clone().into())
        .await?;

    Ok(())
}
