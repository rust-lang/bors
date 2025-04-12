use crate::database::OctocrabMergeableState;
use crate::github::{GithubRepoName, PullRequestNumber};
use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::time::timeout;

use super::BorsContext;

/// Delay before processing a mergeable queue item for the first time.
const BASE_DELAY: Duration = Duration::from_millis(500);
/// Exponential backoff delay multiplier.
const BACKOFF_MULTIPLIER: f64 = 2.0;
/// Max number of mergeable check retries before giving up.
const MAX_RETRIES: u32 = 5;

#[derive(Debug, Clone)]
pub struct QueuedPullRequest {
    pub pr_number: PullRequestNumber,
    pub repo: GithubRepoName,
}

impl std::fmt::Display for QueuedPullRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/#{}", self.repo, self.pr_number)
    }
}

#[derive(Debug, Clone)]
pub struct MergeableQueueItem {
    pub pull_request: QueuedPullRequest,
    pub attempt: u32,
    pub immediate: bool,
}

#[derive(Debug, Clone)]
struct Item {
    /// When to process item (None = immediate).
    /// Reversed to create min-heap for expirations.
    expiration: Reverse<Option<Instant>>,
    inner: MergeableQueueItem,
}

impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.expiration == other.expiration
    }
}

impl Eq for Item {}

impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Item {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.expiration.cmp(&other.expiration)
    }
}

struct SharedInner {
    queue: Mutex<BinaryHeap<Item>>,
    notify: Notify,
    closed: AtomicBool,
}

#[derive(Clone)]
pub struct MergeableQueueSender {
    inner: Arc<SharedInner>,
}

#[derive(Clone)]
pub struct MergeableQueueReceiver {
    inner: Arc<SharedInner>,
}

pub fn create_mergeable_queue() -> (MergeableQueueSender, MergeableQueueReceiver) {
    let shared = Arc::new(SharedInner {
        queue: Mutex::new(BinaryHeap::new()),
        notify: Notify::new(),
        closed: AtomicBool::new(false),
    });

    (
        MergeableQueueSender {
            inner: shared.clone(),
        },
        MergeableQueueReceiver { inner: shared },
    )
}

impl MergeableQueueSender {
    pub fn close(&self) {
        self.inner.closed.store(true, Ordering::Relaxed);
        self.inner.notify.notify_waiters();
    }

    pub fn enqueue(&self, repo: GithubRepoName, pr_number: PullRequestNumber) {
        let expiration = Some(Instant::now() + BASE_DELAY);

        self.insert_item(
            MergeableQueueItem {
                pull_request: QueuedPullRequest { pr_number, repo },
                attempt: 1,
                immediate: false,
            },
            expiration,
        );
    }

    #[allow(dead_code)]
    pub fn enqueue_now(&self, pr_number: PullRequestNumber, repo: GithubRepoName) {
        self.insert_item(
            MergeableQueueItem {
                pull_request: QueuedPullRequest { pr_number, repo },
                attempt: 1,
                immediate: true,
            },
            None,
        );
    }

    pub fn enqueue_retry(&self, queue_item: MergeableQueueItem) {
        let next_attempt = queue_item.attempt + 1;
        let delay = calculate_exponential_backoff(BASE_DELAY, BACKOFF_MULTIPLIER, next_attempt);
        let expiration = Some(Instant::now() + delay);

        self.insert_item(
            MergeableQueueItem {
                pull_request: queue_item.pull_request,
                attempt: next_attempt,
                immediate: queue_item.immediate,
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
        // 2. The queue was empty before insertion (reader is waiting)
        // 3. The current item is an immediate item
        let should_notify = queue.is_empty() || expiration.is_none() || has_earlier_expiration;

        queue.push(Item {
            expiration: Reverse(expiration),
            inner: item,
        });

        if should_notify {
            self.inner.notify.notify_one();
        }
    }
}

impl MergeableQueueReceiver {
    fn peek_inner(&self) -> Result<MergeableQueueItem, Option<Duration>> {
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

    pub async fn dequeue(&self) -> Option<MergeableQueueItem> {
        loop {
            // Closed and empty queue, we're done.
            if self.inner.closed.load(Ordering::Relaxed)
                && self.inner.queue.lock().unwrap().is_empty()
            {
                return None;
            }

            match self.peek_inner() {
                // Item is ready.
                Ok(item) => break Some(item),
                // Item exists but not ready, wait until then or until notified of a higher priority item.
                Err(Some(duration)) => {
                    let _ = timeout(duration, self.inner.notify.notified()).await;
                }
                // Queue is empty, wait until notified of a new item.
                Err(None) => {
                    // If closed, we're done
                    if self.inner.closed.load(Ordering::Relaxed) {
                        return None;
                    }
                    // else, wait until next item.
                    self.inner.notify.notified().await;
                }
            }
        }
    }
}

fn calculate_exponential_backoff(
    base_delay: Duration,
    backoff_multiplier: f64,
    attempt: u32,
) -> Duration {
    let multiplier = backoff_multiplier.powi(attempt as i32 - 1);
    let timeout = (base_delay.as_millis() as f64 * multiplier) as u64;
    Duration::from_millis(timeout)
}

pub async fn handle_mergeable_queue_item(
    ctx: Arc<BorsContext>,
    mq_tx: MergeableQueueSender,
    mq_item: MergeableQueueItem,
) -> anyhow::Result<()> {
    let MergeableQueueItem {
        pull_request,
        attempt,
        ..
    } = mq_item.clone();

    if attempt >= MAX_RETRIES {
        tracing::warn!(
            "Exceeded max mergeable state checks for PR: {}",
            pull_request
        );
        return Ok(());
    }

    let pr_model = match ctx
        .db
        .get_pull_request(&pull_request.repo, pull_request.pr_number)
        .await?
    {
        Some(model) => model,
        None => {
            tracing::error!("PR not found in database: {}", pull_request);
            return Ok(());
        }
    };

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
                "Failed to acquire read lock on repositories: {}",
                err
            ));
        }
    };

    let fetched_pr = repo_state
        .client
        .get_pull_request(pull_request.pr_number)
        .await?;
    let new_mergeable_state = fetched_pr.mergeable_state;

    if new_mergeable_state == OctocrabMergeableState::Unknown {
        tracing::info!(
            "Retrying mergeable state check for PR: {pull_request} ({attempt}/{MAX_RETRIES})",
        );
        mq_tx.enqueue_retry(mq_item);
        return Ok(());
    }

    ctx.db
        .update_pr_mergeable_state(&pr_model, new_mergeable_state.clone().into())
        .await?;

    tracing::debug!("PR {pull_request} `mergeable_state` updated to `{new_mergeable_state:?}`");

    Ok(())
}
