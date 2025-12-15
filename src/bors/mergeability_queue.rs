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

use super::{BorsContext, RepositoryState};
use crate::PgDbClient;
use crate::bors::comment::conflict_comment;
use crate::database::{MergeableState, OctocrabMergeableState, PullRequestModel};
use crate::github::{GithubRepoName, PullRequestNumber};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::AtomicBool;
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

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct MergeabilityCheckPriority(u32);

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PullRequestData {
    repo: GithubRepoName,
    pr_number: PullRequestNumber,
}

impl std::fmt::Display for PullRequestData {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}/#{}", self.repo, self.pr_number)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct PullRequestToCheck {
    pull_request: PullRequestData,
    /// Which attempt to check mergeability are we processing?
    attempt: u32,
    /// Priority of the item. Higher priority items are handled before lower priority items.
    /// Notably, even if a higher priority item has an expiration set, and a lower priority item
    /// doesn't, the higher priority item will be handled first, if it's expiration has elapsed.
    priority: MergeabilityCheckPriority,
    /// Was the pull request mergeable *before* being put into the mergeability queue?
    was_previously_mergeable: bool,
    /// Merged pull request that *might* have caused a merge conflict for this PR.
    conflict_source: Option<PullRequestNumber>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct QueueItem {
    entry: PullRequestToCheck,
    /// When to process item (None = immediate is ordered before Some).
    expiration: Option<Instant>,
}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Order shutdowns after all other kinds of messages
        let (
            QueueItem {
                entry: entry1,
                expiration: expire1,
            },
            QueueItem {
                entry: entry2,
                expiration: expire2,
            },
        ) = (self, other);
        {
            // Note: we don't order by priority, because items with a different priority should
            // be in different queues altogether. Let's check that here
            assert_eq!(entry1.priority, entry2.priority);

            // Order by expiration => None before Some
            expire1
                .cmp(expire2)
                // Then order by PR number
                .then_with(|| entry1.pull_request.cmp(&entry2.pull_request))
                // And finally by attempt
                .then_with(|| entry1.attempt.cmp(&entry2.attempt))
        }
    }
}

struct SharedInner {
    /// List of queues ordered by priority.
    /// Reversed is used to create a min-heap (return items that expire the soonest).
    queues: Mutex<BTreeMap<MergeabilityCheckPriority, BinaryHeap<Reverse<QueueItem>>>>,
    notify: Notify,
    shutdown_requested: AtomicBool,
}

#[derive(Clone)]
pub struct MergeabilityQueueSender {
    inner: Arc<SharedInner>,
}

pub struct MergeabilityQueueReceiver {
    inner: Arc<SharedInner>,
}

pub fn create_mergeability_queue() -> (MergeabilityQueueSender, MergeabilityQueueReceiver) {
    let shared = Arc::new(SharedInner {
        queues: Mutex::new(BTreeMap::new()),
        notify: Notify::new(),
        shutdown_requested: AtomicBool::new(false),
    });

    (
        MergeabilityQueueSender {
            inner: shared.clone(),
        },
        MergeabilityQueueReceiver { inner: shared },
    )
}

impl MergeabilityQueueSender {
    /// Shutdown the mergeability queue.
    pub fn shutdown(&self) {
        // Store shutdown flag
        self.inner
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        // and wake the receiver for immediate processing.
        self.inner.notify.notify_one();
    }

    /// Enqueues the given PR for a mergeability check.
    /// It will be repeatedly fetched in the background from the GH API, until we can figure out
    /// its mergeability status.
    pub fn enqueue_pr(&self, model: &PullRequestModel, conflict_source: Option<PullRequestNumber>) {
        // Prioritize approved PRs, so that they are checked first.
        // Those are the most important to check, because after a merge of some other PR, they
        // might be kicked out of the merge queue if they are no longer mergeable.
        let priority = if model.is_approved() { 1 } else { 0 };
        self.enqueue(
            &model.repository,
            model.number,
            MergeabilityCheckPriority(priority),
            match model.mergeable_state {
                MergeableState::Mergeable => true,
                MergeableState::HasConflicts => false,
                MergeableState::Unknown => false,
            },
            conflict_source,
        );
    }

    pub fn enqueue(
        &self,
        repo: &GithubRepoName,
        number: PullRequestNumber,
        priority: MergeabilityCheckPriority,
        was_previously_mergeable: bool,
        conflict_source: Option<PullRequestNumber>,
    ) {
        self.insert_pr_item(
            PullRequestToCheck {
                pull_request: PullRequestData {
                    pr_number: number,
                    repo: repo.clone(),
                },
                attempt: 1,
                priority,
                was_previously_mergeable,
                conflict_source,
            },
            None,
        );
    }

    fn enqueue_retry(&self, pr_item: PullRequestToCheck) {
        let PullRequestToCheck {
            pull_request,
            attempt,
            priority,
            was_previously_mergeable,
            conflict_source,
        } = pr_item;

        // First attempt = BASE_DELAY
        // Second attempt = BASE_DELAY * 2
        // Third attempt = BASE_DELAY * 3
        // etc.
        let delay = BASE_DELAY * attempt;
        let expiration = Some(Instant::now() + delay);
        let next_attempt = attempt + 1;

        self.insert_pr_item(
            PullRequestToCheck {
                pull_request,
                attempt: next_attempt,
                priority,
                was_previously_mergeable,
                conflict_source,
            },
            expiration,
        );
    }

    fn insert_pr_item(&self, item: PullRequestToCheck, expiration: Option<Instant>) {
        let mut queues = self.inner.queues.lock().unwrap();

        // Make sure that we don't ever put the same pull request twice into the queues
        // This might seem a bit inefficient, but linearly iterating through e.g. 1000 PRs should
        // be fine.
        // We could maybe reset the attempt counter of the PR if it's "refreshed" from the outside,
        // but that would require using e.g. Cell to mutate the attempt counter through &, which
        // doesn't seem necessary at the moment.
        if queues
            .iter()
            .flat_map(|(_, queue)| queue.iter())
            .any(|entry| entry.0.entry.pull_request == item.pull_request)
        {
            return;
        }

        let queue = queues.entry(item.priority).or_default();

        // Notify when:
        // 1. The current item expires sooner than the head of the queue or has higher
        // priority than it.
        let has_earlier_expiration =
            queue
                .peek()
                .is_some_and(|head| match (expiration, head.0.expiration) {
                    (Some(new_exp), Some(head_exp)) => new_exp < head_exp,
                    _ => false,
                });
        // 2. The queue was empty before insertion (reader might be waiting)
        let should_notify = queue.is_empty() || expiration.is_none() || has_earlier_expiration;

        queue.push(Reverse(QueueItem {
            expiration,
            entry: item,
        }));

        if should_notify {
            self.inner.notify.notify_one();
        }
    }
}

impl MergeabilityQueueReceiver {
    /// Get the next item from the queue.
    pub async fn dequeue(&self) -> Option<(PullRequestToCheck, MergeabilityQueueSender)> {
        loop {
            match self.peek_inner() {
                // Shutdown signal
                Ok(None) => break None,
                // Item is ready.
                Ok(Some(QueueItem {
                    entry,
                    expiration: _,
                })) => {
                    break Some((
                        entry,
                        MergeabilityQueueSender {
                            inner: self.inner.clone(),
                        },
                    ));
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
    fn peek_inner(&self) -> Result<Option<QueueItem>, Option<Duration>> {
        let now = Instant::now();
        let mut queues = self.inner.queues.lock().unwrap();

        // Shutdown requested, report end
        if self
            .inner
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Ok(None);
        }

        let mut sleep_time = None;

        // Iterate queues from highest priority to lowest, trying to find an item to be dequeued
        for (_, queue) in queues.iter_mut().rev() {
            match queue.peek() {
                // Immediate item, ready for processing.
                Some(Reverse(QueueItem {
                    expiration: None,
                    entry: _,
                })) => {
                    let item = queue.pop().unwrap().0;
                    return Ok(Some(item));
                }
                // Expiration has passed, ready for processing.
                Some(Reverse(QueueItem {
                    expiration: Some(expiration),
                    ..
                })) if *expiration <= now => {
                    let item = queue.pop().unwrap().0;
                    return Ok(Some(item));
                }
                // Scheduled for the future, wait until it's ready.
                Some(Reverse(QueueItem {
                    expiration: Some(expiration),
                    ..
                })) => {
                    let wait_time = *expiration - now;
                    sleep_time = match sleep_time {
                        None => Some(wait_time),
                        Some(t) => Some(t.min(wait_time)),
                    };
                }
                // Empty queue, move on to the next one.
                None => {}
            }
        }
        match sleep_time {
            Some(time) => {
                // Some queues are waiting, report sleep time
                Err(Some(time))
            }
            None => {
                // All queues are empty
                Err(None)
            }
        }
    }
}

pub async fn check_mergeability(
    ctx: Arc<BorsContext>,
    mq_tx: MergeabilityQueueSender,
    mq_item: PullRequestToCheck,
) -> anyhow::Result<()> {
    let PullRequestToCheck {
        ref pull_request,
        ref attempt,
        priority: _,
        conflict_source,
        was_previously_mergeable,
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

    let new_mergeable_state: MergeableState = new_mergeable_state.into();
    // We know the mergeability status, so update it in the DB
    ctx.db
        .set_pr_mergeable_state(
            repo_state.repository(),
            fetched_pr.number,
            new_mergeable_state.clone(),
        )
        .await?;

    if new_mergeable_state == MergeableState::HasConflicts && was_previously_mergeable {
        handle_pr_conflict(&repo_state, &ctx.db, pull_request, conflict_source).await?;
    }

    Ok(())
}

async fn handle_pr_conflict(
    repo_state: &RepositoryState,
    db: &PgDbClient,
    pr: &PullRequestData,
    conflict_source: Option<PullRequestNumber>,
) -> anyhow::Result<()> {
    tracing::info!("Pull request {pr:?} was likely unmergeable (source: {conflict_source:?})");

    if !repo_state.config.load().report_merge_conflicts {
        tracing::info!("Reporting merge conflicts is disabled, not doing anything further");
        return Ok(());
    }

    let Some(pr) = db.get_pull_request(&pr.repo, pr.pr_number).await? else {
        return Err(anyhow::anyhow!("Pull request {pr:?} was not found"));
    };

    // Unapprove PR
    let unapproved = if pr.is_approved() {
        db.unapprove(&pr).await?;
        true
    } else {
        false
    };
    repo_state
        .client
        .post_comment(pr.number, conflict_comment(conflict_source, unapproved))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::mergeability_queue::{
        BASE_DELAY, MergeabilityCheckPriority, MergeabilityQueueSender, PullRequestData,
        PullRequestToCheck, create_mergeability_queue,
    };
    use crate::github::{GithubRepoName, PullRequestNumber};
    use crate::tests::default_repo_name;

    #[tokio::test]
    async fn order_by_pr_number() {
        let (tx, rx) = create_mergeability_queue();
        item(3).enqueue(&tx);
        item(1).enqueue(&tx);
        item(2).enqueue(&tx);

        for expected in [1, 2, 3] {
            assert_eq!(
                rx.dequeue().await.unwrap().0.pull_request.pr_number.0,
                expected
            );
        }
    }

    #[tokio::test]
    async fn immediate_before_delayed() {
        let (tx, rx) = create_mergeability_queue();
        item(10).enqueue_retry(&tx, 1);
        item(2).enqueue(&tx);

        for expected in [2, 10] {
            assert_eq!(
                rx.dequeue().await.unwrap().0.pull_request.pr_number.0,
                expected
            );
        }
    }

    #[tokio::test]
    async fn deduplicate_duplicated_pr() {
        let (tx, rx) = create_mergeability_queue();
        // Make sure that we don't handle the same PR multiple times
        item(5).enqueue(&tx);
        item(5).enqueue(&tx);
        item(5).enqueue_retry(&tx, 1);

        rx.dequeue().await.unwrap();
        let res = tokio::time::timeout(BASE_DELAY * 2, rx.dequeue()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn deduplicate_duplicated_pr_across_priorities() {
        let (tx, rx) = create_mergeability_queue();
        // Make sure that we don't handle the same PR multiple times
        item(5).enqueue(&tx);
        item(5).priority(2).enqueue(&tx);
        item(5).enqueue_retry(&tx, 1);

        rx.dequeue().await.unwrap();
        let res = tokio::time::timeout(BASE_DELAY * 2, rx.dequeue()).await;
        assert!(res.is_err());
    }

    #[tokio::test]
    async fn order_by_priority() {
        let (tx, rx) = create_mergeability_queue();
        item(3).priority(1).enqueue(&tx);
        item(1).enqueue(&tx);

        for expected in [3, 1] {
            assert_eq!(
                rx.dequeue().await.unwrap().0.pull_request.pr_number.0,
                expected
            );
        }
    }

    #[tokio::test]
    async fn order_by_priority_expiration() {
        let (tx, rx) = create_mergeability_queue();
        item(3).priority(1).enqueue_retry(&tx, 1);
        item(1).enqueue(&tx);
        item(2).enqueue(&tx);

        assert_eq!(rx.dequeue().await.unwrap().0.pull_request.pr_number.0, 1);

        // Wait for the higher priority item to have expiration set
        tokio::time::sleep(BASE_DELAY * 2).await;

        // And check that it is returned before the immediate item with lower priority
        for expected in [3, 2] {
            assert_eq!(
                rx.dequeue().await.unwrap().0.pull_request.pr_number.0,
                expected
            );
        }
    }

    fn item(pr: u64) -> ItemToCheck {
        ItemToCheck::default().pr(pr)
    }

    struct ItemToCheck {
        repo: GithubRepoName,
        number: PullRequestNumber,
        priority: MergeabilityCheckPriority,
    }

    impl ItemToCheck {
        fn pr(mut self, number: u64) -> Self {
            self.number = PullRequestNumber(number);
            self
        }
        fn priority(mut self, priority: u32) -> Self {
            self.priority = MergeabilityCheckPriority(priority);
            self
        }

        fn enqueue(&self, tx: &MergeabilityQueueSender) {
            tx.enqueue(&self.repo, self.number, self.priority, false, None);
        }

        fn enqueue_retry(&self, tx: &MergeabilityQueueSender, attempt: u32) {
            tx.enqueue_retry(PullRequestToCheck {
                pull_request: PullRequestData {
                    repo: self.repo.clone(),
                    pr_number: self.number,
                },
                attempt,
                priority: self.priority,
                was_previously_mergeable: false,
                conflict_source: None,
            });
        }
    }

    impl Default for ItemToCheck {
        fn default() -> Self {
            Self {
                repo: default_repo_name(),
                number: PullRequestNumber(1),
                priority: MergeabilityCheckPriority::default(),
            }
        }
    }
}
