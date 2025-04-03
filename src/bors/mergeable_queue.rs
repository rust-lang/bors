use futures::task::noop_waker;
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant},
};
use tokio_util::time::DelayQueue;

use crate::{
    database::OctocrabMergeableState,
    github::{GithubRepoName, PullRequestNumber},
};

use super::BorsContext;

type ScheduledPullRequests = Arc<Mutex<DelayQueue<MergeableCheckEntry>>>;
type TrackedPullRequests = Arc<Mutex<HashMap<MergeableCheckKey, Instant>>>;

/// How long to wait before checking mergeable state (for first time).
const INITIAL_DELAY: Duration = Duration::from_secs(2);
/// How long to wait before rechecking mergeable state if still unknown.
const RETRY_DELAY: Duration = Duration::from_secs(5);
/// Max number of mergeable state rechecks before giving up.
const MAX_RETRIES: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct MergeableCheckKey {
    pub repository: GithubRepoName,
    pub pr_number: PullRequestNumber,
}

impl fmt::Display for MergeableCheckKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.repository, self.pr_number)
    }
}

#[derive(Debug, Clone)]
struct MergeableCheckEntry {
    pub key: MergeableCheckKey,
    pub retry_count: usize,
}

impl MergeableCheckEntry {
    pub fn new(key: MergeableCheckKey) -> Self {
        Self {
            key,
            retry_count: 0,
        }
    }
}

/// A queue system for updating PR mergeable states.
///
/// This queue allows for PRs to be scheduled for a mergeable_state recheck after a delay.
/// - PRs are initially checked after a short delay
/// - If still unknown, check will be retried with delays
pub struct MergeableQueue {
    scheduled_prs: ScheduledPullRequests,
    tracked_prs: TrackedPullRequests,
    shutdown_tx: Option<mpsc::Sender<()>>,
    ctx: Arc<BorsContext>,
}

impl MergeableQueue {
    pub fn new(ctx: Arc<BorsContext>) -> Self {
        Self {
            scheduled_prs: Arc::new(Mutex::new(DelayQueue::new())),
            tracked_prs: Arc::new(Mutex::new(HashMap::new())),
            shutdown_tx: None,
            ctx,
        }
    }

    pub fn start(&mut self) {
        let scheduled_prs = Arc::clone(&self.scheduled_prs);
        let tracked_prs = Arc::clone(&self.tracked_prs);
        let ctx = Arc::clone(&self.ctx);

        let (shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);
        self.shutdown_tx = Some(shutdown_tx);

        tracing::info!("Mergeable queue worker started");

        tokio::spawn(async move {
            loop {
                let sleep_duration = {
                    let queue = scheduled_prs.lock().unwrap();

                    if let Some(key) = queue.peek() {
                        let now = Instant::now();
                        let deadline = queue.deadline(&key);
                        if deadline <= now {
                            // Ready for immediate processing.
                            Duration::from_millis(0)
                        } else {
                            // Sleep until the next item's deadline.
                            std::cmp::min(deadline - now, Duration::from_secs(1))
                        }
                    } else {
                        // Queue is empty, take a nap.
                        Duration::from_secs(1)
                    }
                };

                tokio::select! {
                    _ = tokio::time::sleep(sleep_duration) => {
                        Self::process_expired_entries(
                            Arc::clone(&scheduled_prs),
                            Arc::clone(&tracked_prs),
                            Arc::clone(&ctx)
                        ).await;
                    }
                    _ = shutdown_rx.recv() => {
                        tracing::info!("Mergeable queue worker shutdown");
                        break;
                    }
                }
            }
        });
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.try_send(());
            tracing::info!("Shutting down mergeable queue worker");
        }
    }

    pub fn enqueue(&self, repository: GithubRepoName, pr_number: PullRequestNumber) {
        let key = MergeableCheckKey {
            repository,
            pr_number,
        };

        let mut tracked_prs = self.tracked_prs.lock().unwrap();
        let now = Instant::now();

        if let Some(expire_time) = tracked_prs.get(&key) {
            if *expire_time > now {
                tracing::debug!("Ignoring PR {key} as already in mergeable queue");
                return;
            }
        }

        let expire_time = now + INITIAL_DELAY;
        tracked_prs.insert(key.clone(), expire_time);

        self.scheduled_prs
            .lock()
            .unwrap()
            .insert(MergeableCheckEntry::new(key.clone()), INITIAL_DELAY);

        tracing::info!("PR {key} queued for mergeable state check");
    }

    async fn process_expired_entries(
        scheduled_prs: Arc<Mutex<DelayQueue<MergeableCheckEntry>>>,
        tracked_prs: Arc<Mutex<HashMap<MergeableCheckKey, Instant>>>,
        ctx: Arc<BorsContext>,
    ) {
        loop {
            let maybe_entry = {
                let mut queue = scheduled_prs.lock().unwrap();

                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);

                match queue.poll_expired(&mut cx) {
                    Poll::Ready(Some(expired)) => Some(expired.into_inner()),
                    _ => None,
                }
            };

            if let Some(entry) = maybe_entry {
                Self::process_pr(
                    Arc::clone(&scheduled_prs),
                    Arc::clone(&tracked_prs),
                    Arc::clone(&ctx),
                    entry,
                )
                .await;
            } else {
                break;
            }
        }
    }

    async fn process_pr(
        scheduled_prs: ScheduledPullRequests,
        tracked_prs: TrackedPullRequests,
        ctx: Arc<BorsContext>,
        entry: MergeableCheckEntry,
    ) {
        let MergeableCheckEntry { key, retry_count } = entry;
        tracing::info!("Processing mergeable state for PR {key}, retry: {retry_count}",);

        let pr_model = match ctx
            .db
            .get_pull_request(&key.repository, key.pr_number)
            .await
        {
            Ok(Some(pr_model)) => pr_model,
            Ok(None) => {
                tracing::warn!("PR {key} not found in database");
                tracked_prs.lock().unwrap().remove(&key);
                return;
            }
            Err(err) => {
                tracing::error!("Failed to get PR {key} from database: {err}");
                tracked_prs.lock().unwrap().remove(&key);
                return;
            }
        };

        if retry_count >= MAX_RETRIES {
            tracing::warn!("Exceeded max retries ({MAX_RETRIES}) for PR {key}. Giving up.",);
            tracked_prs.lock().unwrap().remove(&key);
            return;
        }

        let repo_state = ctx
            .repositories
            .read()
            .unwrap()
            .get(&key.repository)
            .cloned()
            .unwrap();
        let fetched_pr = repo_state
            .client
            .get_pull_request(key.pr_number)
            .await
            .unwrap();

        if fetched_pr.mergeable_state == OctocrabMergeableState::Unknown {
            let now = Instant::now();
            let expire_time = now + RETRY_DELAY;
            let new_retry_count = retry_count + 1;
            tracked_prs.lock().unwrap().insert(key.clone(), expire_time);

            scheduled_prs.lock().unwrap().insert(
                MergeableCheckEntry {
                    key: key.clone(),
                    retry_count: new_retry_count,
                },
                RETRY_DELAY,
            );

            tracing::info!(
                "Requeued PR {key} for mergeable state check, retry: {}",
                new_retry_count
            );
            return;
        }

        ctx.db
            .update_pr_mergeable_state(&pr_model, fetched_pr.mergeable_state.clone().into())
            .await
            .unwrap();

        tracked_prs.lock().unwrap().remove(&key);
        tracing::debug!(
            "PR {key} mergeable state updated to {:?}",
            fetched_pr.mergeable_state
        );
    }
}

impl Drop for MergeableQueue {
    fn drop(&mut self) {
        self.stop();
    }
}
