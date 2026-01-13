//! # Mergeability checking
//! Having up-to-date information about mergeability is tricky. We want to balance several needs:
//! - Avoid ignoring a PR that seems to be unmergeable, but in reality could be merged.
//! - Recover from missed webhooks and bors being offline, so that we do not remember a PR in a
//!   certain mergeability state that is stale, without ever updating it again.
//! - Avoid race conditions that could send multiple "PR is unmergeable" comments per one switch
//!   from mergeable to unmergeable.
//! - Report PRs becoming unmergeable as quickly as possible, ideally with the precise PR that
//!   caused the merge conflict.
//! - Do not report PRs becoming unmergeable in case a user action on the PR has happened in-between
//!   the event that caused the unmergeability and the unmergeability notification being posted.
//! - Do all of the above without making too many unnecessary GitHub API calls.
//!
//! ## Desired properties
//! There are some desires properties and general rules that we want to uphold. Those are documented
//! below.
//!
//! ### Avoiding stale mergeability status
//! We want to ensure that if the mergeability of a PR becomes stale (which happens after a
//! commit being pushed to the PR or its base branch), we will eventually resolve its mergeability
//! from GH in the future. This should happen even if bors stops executing, misses some webhooks
//! or if GitHub has transient issues and it cannot return accurate mergeability even after N
//! attempts. Even if all else fails, the status should be updated at least if you send any bors
//! comment on the pull request. We explicitly do not want to force users to have to do something
//! like `@bors sync` or similar.
//!
//! ### Unapproving
//! We want to ensure that when we learn about a PR that **becomes** unmergeable, we will
//! unapprove it. Even though it could become mergeable again in the future even without a push to
//! that PR, that is quite rare, and unapproving the PR outright is a safer solution.
//!
//! ### Unmergeable PR notification
//! In some cases, when a PR becomes unmergeable, we want to notify its author by sending a comment
//! to the PR. We want to avoid notifying the same PR multiple times if it didn't become mergeable
//! since the last notification, and ideally we would like to also specify the PR which caused the
//! merge conflict (the "conflict source").
//!
//! We do not want to send these comments in response to user actions (e.g. a PR being pushed to),
//! but we still want to update the mergeability status in that case, if we know it from GitHub.
//!
//! ## Design
//! With all the important properties described, let's talk about how mergeability checking works
//! in bors.
//!
//! ### Mergeability queue
//! To reload the mergeability of PRs, we use a mergeability queue, which is implemented in this
//! module. Sadly, the mergeability status cannot be determined directly from GitHub, because it is
//! computed by a background job (see https://docs.github.com/en/rest/guides/using-the-rest-api-to-interact-with-your-git-database).
//! Instead, we have to poll GitHub repeatedly, which is what the queue does.
//!
//! When you add a PR to the mergeability queue, it will then immediately contact the GH API
//! (by GETing the PR from GitHub), which should start the GitHub background job. After that, the
//! PR will be checked in increasing intervals (after 5s, then after 10s, then after 15s, etc.),
//! until we either get a known mergeability status from GH or until we run out of retries.
//!
//! A PR can be added to the queue multiple times. If it was previously in the queue, nothing will
//! happen (even if you add it there e.g. with a different conflict source).
//!
//! ### Learning about mergeability changes
//! There are three main sources of events that can make bors realize that mergeability might
//! have changed:
//! A) When a commit is pushed to the base branch of a PR, we know that its mergeability status
//!    becomes stale. At this point, we also know the most recently merged PR, and we can provide
//!    precise information about it in the unmergeability notifications. When this happens, we
//!    put all non-closed PRs into the mergeability queue.
//! B) When the merge queue tries to merge a PR, it can receive an actual merge conflict when
//!    performing a merge or it can learn from the GitHub API that the PR is currently unmergeable.
//! C) When we receive a PR-related webhook (PR edited, assignees changed, commit pushed to a PR,
//!    comment sent to a PR) based on user action, the webhook contains information about the
//!    current mergeability status of the PR. In this case we try to update the mergeability status
//!    in the DB, but we do not send the notification, because it could lead to unnecessary spam
//!    (as the user is already doing something with the PR!).
//!
//! There is a particular set of notable race conditions that we document:
//! - **A1 followed by A2 before A1 ends**. In this case, the conflict source could be imprecise,
//!   because the mergeability queue might not correctly select the source between A1 and A2.
//!   This could probably be fixed by having a separate DB table that could remember multiple
//!   conflict sources per PR, but that seems overkill.
//!   So we explicitly do not care about this race condition. Two base branch pushes on
//!   rust-lang/rust happen very infrequently (usually not more often than once every 3 hours),
//!   while we expect the full mergeability check to finish within 5-10 minutes. Thus this should
//!   not occur ~ever.
//! - **A followed by a bors redeploy**. In this case, the conflict source could be forgotten, and
//!   we can only display a generic unmergeability message. It could be solved by the same approach
//!   as hinted above, but same as above, we do not care about this. If it happens, the notification
//!   will just not contain the conflict source.
//! - **A followed by C before A ends**. Assume that a commit is pushed to `main`, which triggers
//!   enqueuing of PR #1 into the mergeability queue. It can take several minutes before A finishes.
//!   If during that time PR #1 receives a webhook (it gets edited or receives a bors comment),
//!   we can do two things:
//!   1. Immediately send the conflict notification, if needed.
//!   2. Just update the mergeability status in the DB, but do not post the notification.
//!
//!   We do 2., with the assumption that if the user does something with the PR, they are already
//!   observing it, and they do not need to receive an additional notification. In particular,
//!   if we get a push to a base branch followed by A, and then a push to the PR, which causes a
//!   conflict, which happens before A finishes, then we do not want to spam the user with a
//!   notification.
//! - **A followed by B before A ends**. If the merge queue attempts to start an auto build for a
//!   pull request that is still being checked in the mergeability queue, the mergeability check
//!   could race. If the GitHub mergeability status is unknown, the merge queue will wait until the
//!   mergeability check finishes. If the mergeability status is known (and unmergeable), the merge
//!   queue will immediately unapprove the PR to remove it from the merge queue, and it will send
//!   the notification at the same time. If the PR was already enqueued in the mergeability check
//!   with a conflict source at the same time, it will take the conflict source from the
//!   mergeability queue, to avoid losing the conflict source.
//! - **B running before A**. Similar to the previous race condition. In this case, we will post the
//!   lower quality conflict notification, without the conflict source, as it has not been
//!   determined yet. We could fix this by enqueuing all PRs targeting the given base branch
//!   immediately after a PR being merged (as we expect bors to do all merges on the repository).
//!   However, it seems a bit dangerous, because if we start asking GitHub for mergeability
//!   "too soon", it could return stale data (in theory, this is untested!). If we only start
//!   checking after we receive the "base branch pushed" webhook, it seems to work fine so far.
//!
//! Some additional comments based on the above:
//! - We do not persist the conflict source in the DB, so it is best-effort. If bors is redeployed,
//!   any conflict sources in the mergeability queue are lost.
//! - We persist the "staleness" of mergeability, so that we can reload it in the future even after
//!   bors redeploys.
//! - If we cannot reload the mergeability for some reason, we will still update it after a PR
//!   webhook comes in, to have a way of staying up to date.
//!
//! All of that leads to the above design:
//! The mergeability state of a PR is stored in the DB in the following two columns:
//! - The `mergeable_state` column contains the last mergeability status known from GitHub
//!   (`mergeable`/`has_conflicts`/`unknown`).
//!   - The `unknown` variant is set for newly opened PRs, PRs receiving a push, etc.
//! - The `mergeable_state_is_stale` column is a boolean that specifies that `mergeable_state`
//!   might be out of date, and we should attempt to reload the mergeability status for the PR
//!   from GitHub.
//!   - In a previous design, we used to only remember `mergeable_state`. However, this meant that
//!     to mark a PR as being stale, we had to overwrite its last known mergeability status. This
//!     was causing race conditions when creating unmergeability comments, because we didn't store
//!     the previously known status in the DB, but only in memory. By storing the status in the DB
//!     separately from the staleness information, we can atomically read the previous mergeability
//!     state and replace it with a new one, and then send a notification only if the old state was
//!     `mergeable`.
//!
//! We say that a PR has *stale mergeability* if its `mergeable_state` column is `unknown` OR its
//! `mergeable_state_is_stale` column is `true`.
//!
//! ### Updating mergeability status
//! When we update the mergeable state in the merge/mergeability queue, we atomically swap the new
//! state with the old in the DB, and clear the stale flag. Then, if the new state is unmergeable,
//! and the old was mergeable, we send the notification. This ensures that we do not send the
//! notification twice without the state switching to the mergeable state in the meantime.
//!
//! When we update the mergeable state in response to user actions (PR pushed to, PR comment, etc.),
//! we update `mergeable_state` directly, to avoid posting a notification.
//!
//! ### Reloading mergeability
//! When bors starts, and periodically every N minutes, it will try to reload the mergeability of
//! PRs that have stale mergeability, and just to be sure also of all approved PRs, as the
//! mergeability status is important for merge queue sorting.
//!
//! ### Unapproving
//! Whenever we can, we mark PRs that become unmergeable as unapproved. However, we might not always
//! observe all switches to the unmergeable status. So the merge queue also has to assume the rare
//! possibility of an approved PR that is unmergeable.

use super::{BorsContext, PullRequestStatus, RepositoryState};
use crate::PgDbClient;
use crate::bors::comment::merge_conflict_comment;
use crate::bors::handlers::unapprove_pr;
use crate::bors::labels::handle_label_trigger;
use crate::database::{MergeableState, OctocrabMergeableState, PullRequestModel};
use crate::github::{GithubRepoName, LabelTrigger, PullRequest, PullRequestNumber};
use std::cmp::Reverse;
use std::collections::{BTreeMap, BinaryHeap};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;
use tokio::time::timeout;
use tracing::Instrument;

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
    /// Return the size of the mergeability queue.
    #[cfg(test)]
    pub fn queue_size(&self) -> usize {
        self.inner
            .queues
            .lock()
            .unwrap()
            .values()
            .map(|q| q.len())
            .sum::<usize>()
    }

    /// Return the PRs currently in the mergeability queue.
    #[cfg(test)]
    pub fn get_queue_prs(&self) -> Vec<PullRequestToCheck> {
        self.inner
            .queues
            .lock()
            .unwrap()
            .iter()
            .flat_map(|(_, q)| {
                let mut queue = q.clone();
                let mut items = vec![];
                while let Some(item) = queue.pop() {
                    items.push(item.0.entry);
                }
                items
            })
            .collect()
    }

    /// Shutdown the mergeability queue.
    #[cfg(test)]
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
    ///
    /// If the PR is already in the queue, it will stay there.
    /// Notably, its conflict source will *not* get overridden by `None` if it was set before.
    pub fn enqueue_pr(&self, model: &PullRequestModel, conflict_source: Option<PullRequestNumber>) {
        // Prioritize approved PRs, so that they are checked first.
        // Those are the most important to check, because after a merge of some other PR, they
        // might be kicked out of the merge queue if they are no longer mergeable.
        let priority = if model.is_approved() { 1 } else { 0 };
        self.enqueue(
            &model.repository,
            model.number,
            MergeabilityCheckPriority(priority),
            conflict_source,
        );
    }

    /// Try to return an existing conflict source for the given PR.
    pub fn get_conflict_source(&self, model: &PullRequestModel) -> Option<PullRequestNumber> {
        let pr_data = PullRequestData {
            repo: model.repository.clone(),
            pr_number: model.number,
        };
        let queues = self.inner.queues.lock().unwrap();
        queues
            .iter()
            .flat_map(|(_, queue)| queue.iter())
            .find(|entry| entry.0.entry.pull_request == pr_data)
            .and_then(|entry| entry.0.entry.conflict_source)
    }

    pub fn enqueue(
        &self,
        repo: &GithubRepoName,
        number: PullRequestNumber,
        priority: MergeabilityCheckPriority,
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

        // Shutdown requested, report end if we have no more items to process
        if self
            .inner
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
            && queues.iter().all(|(_, q)| q.is_empty())
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
    } = mq_item;

    if *attempt >= MAX_RETRIES {
        tracing::warn!("Exceeded max mergeable state attempts for PR: {pull_request}");
        return Ok(());
    }

    let repo_state = ctx.get_repo(&pull_request.repo)?;

    // Load the PR from GitHub.
    // - If the PR's mergeability is unknown, and the GH background job hasn't been started yet,
    //   this PR fetch will trigger its start.
    // - If the PR mergeability is known, we will be able to read it and update it in the DB.
    let fetched_pr = repo_state
        .client
        .get_pull_request(pull_request.pr_number)
        .await?;
    let new_mergeable_state = fetched_pr.mergeable_state.clone();

    // We don't know the mergeability state yet. Retry the PR after some delay
    if new_mergeable_state == OctocrabMergeableState::Unknown {
        match &fetched_pr.status {
            PullRequestStatus::Open | PullRequestStatus::Draft => {
                tracing::info!("Mergeability status unknown, scheduling retry.");
                mq_tx.enqueue_retry(mq_item);
            }
            PullRequestStatus::Closed | PullRequestStatus::Merged => {
                tracing::info!("Mergeability status unknown, but pull request is no longer open.");
            }
        }

        return Ok(());
    } else if let Some(db_pr) = ctx
        .db
        .get_pull_request(repo_state.repository(), fetched_pr.number)
        .await?
    {
        update_pr_with_known_mergeability(
            &repo_state,
            &ctx.db,
            &fetched_pr,
            &db_pr,
            conflict_source,
        )
        .await?;
    } else {
        tracing::warn!("Cannot find DB pull request for {fetched_pr:?}");
    }

    Ok(())
}

/// This method should be called once we learn about the mergeability status of a PR from GitHub
/// (or through other means).
///
/// If needed, it will update the mergeability status in the DB and unapprove the PR and send a
/// PR comment if the PR became unmergeable.
///
/// Should ONLY be called when the mergeability status from GitHub is known.
#[tracing::instrument(skip_all)]
pub async fn update_pr_with_known_mergeability(
    repo: &RepositoryState,
    db: &PgDbClient,
    gh_pr: &PullRequest,
    db_pr: &PullRequestModel,
    conflict_source: Option<PullRequestNumber>,
) -> anyhow::Result<()> {
    let new_mergeable_state = MergeableState::from(gh_pr.mergeable_state.clone());
    assert_ne!(new_mergeable_state, MergeableState::Unknown);

    tracing::debug!(
        "Updating mergeability of {}#{} (DB: {:?}, GH: {new_mergeable_state:?})",
        repo.repository(),
        gh_pr.number,
        db_pr.mergeable_status(),
    );

    // Unapprove PR if needed
    let unapproved = if let MergeableState::HasConflicts = new_mergeable_state
        && db_pr.is_approved()
    {
        unapprove_pr(repo, db, db_pr, gh_pr).await?;
        true
    } else {
        false
    };

    // We update what is in the DB.
    // Note that we do this even if the DB state already matched what comes from GitHub, to
    // possibly clear the "mergeable_is_stale" flag.
    let previous_mergeable_state = db
        .set_pr_mergeable_state(repo.repository(), db_pr.number, new_mergeable_state.clone())
        .await?;

    if !repo.config.load().report_merge_conflicts {
        tracing::debug!("Reporting merge conflicts is disabled, not doing anything further");
        return Ok(());
    }
    tracing::debug!(
        "Previous mergeability status: {previous_mergeable_state:?}, new status: {new_mergeable_state:?}"
    );

    if new_mergeable_state == MergeableState::HasConflicts
        && previous_mergeable_state == MergeableState::Mergeable
    {
        handle_label_trigger(repo, gh_pr, LabelTrigger::Conflict).await?;
        repo.client
            .post_comment(
                db_pr.number,
                merge_conflict_comment(conflict_source, unapproved),
                db,
            )
            .await?;
    }

    Ok(())
}

/// This function should be called when we receive a webhook event **based on user action**, which
/// contains the latest GitHub PR information.
/// Since the webhook reacts to user action, we should not enqueue the PR
pub async fn set_pr_mergeability_based_on_user_action(
    db: &PgDbClient,
    gh_pr: &PullRequest,
    db_pr: &PullRequestModel,
    sender: &MergeabilityQueueSender,
) -> anyhow::Result<()> {
    let mergeable_state = MergeableState::from(gh_pr.mergeable_state.clone());

    let span = tracing::debug_span!(
        "mergeability_user_action",
        "{}#{}",
        db_pr.repository,
        db_pr.number
    );

    // If the status is unknown or it is different than what is in the DB, we enqueue the PR
    if mergeable_state == MergeableState::Unknown {
        // Only update to Unknown if the DB doesn't already have a known conflict state.
        // We should not overwrite HasConflicts with Unknown, as that would lose important
        // information about merge conflicts that should block approval.
        if db_pr.mergeable_status() != MergeableState::HasConflicts {
            // Eagerly update the last known state in the DB. This will essentially avoid posting the
            // conflict notification, because the last state will be unknown.
            db.set_pr_mergeable_state(&db_pr.repository, db_pr.number, mergeable_state)
                .instrument(span)
                .await?;
        }
        sender.enqueue_pr(db_pr, None);
    } else if mergeable_state != db_pr.mergeable_status() {
        span.in_scope(|| {
            tracing::debug!(
            "Encountered mergeability status staleness in response to user action. New state: {mergeable_state:?}, old state: {:?}, approved: {}",
                db_pr.mergeable_status(),
                db_pr.is_approved()
            );
        });

        // If the status is unknown, we assume that the PR will be refreshed soon by some other
        // means, and we do not do anything here
        // Also, do not overwrite HasConflicts with any other state, as that would lose critical
        // merge conflict information needed to block PR approval.
        if db_pr.mergeable_status() != MergeableState::Unknown
            && db_pr.mergeable_status() != MergeableState::HasConflicts
        {
            // If it is not unknown and not HasConflicts, we assume that for some reason this PR
            // has not been updated properly in the DB. We will thus force update the state here,
            // without sending a notification, and without unapproving the PR.
            db.set_pr_mergeable_state(&db_pr.repository, db_pr.number, mergeable_state)
                .instrument(span)
                .await?;
        }
    }
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
            tx.enqueue(&self.repo, self.number, self.priority, None);
        }

        fn enqueue_retry(&self, tx: &MergeabilityQueueSender, attempt: u32) {
            tx.enqueue_retry(PullRequestToCheck {
                pull_request: PullRequestData {
                    repo: self.repo.clone(),
                    pr_number: self.number,
                },
                attempt,
                priority: self.priority,
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
