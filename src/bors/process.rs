use crate::bors::build_queue::{
    BuildQueueReceiver, BuildQueueSender, create_buid_queue, handle_build_queue_event,
};
use crate::bors::merge_queue::{MergeQueueSender, start_merge_queue};
use crate::bors::mergeability_queue::{
    MergeabilityQueueReceiver, MergeabilityQueueSender, check_mergeability,
    create_mergeability_queue,
};
use crate::bors::{handle_bors_global_event, handle_bors_repository_event};
use crate::{BorsContext, BorsGlobalEvent, BorsRepositoryEvent, TeamApiClient};
use anyhow::Error;
use octocrab::Octocrab;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{Instrument, Span};

pub struct BorsProcess {
    pub repository_tx: mpsc::Sender<BorsRepositoryEvent>,
    pub global_tx: mpsc::Sender<BorsGlobalEvent>,
    pub senders: QueueSenders,
    pub bors_process: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    ctx: Arc<BorsContext>,
    gh_client: Octocrab,
    team_api: TeamApiClient,
    merge_queue_max_interval: chrono::Duration,
) -> BorsProcess {
    let (repository_tx, repository_rx) = mpsc::channel::<BorsRepositoryEvent>(1024);
    let (global_tx, global_rx) = mpsc::channel::<BorsGlobalEvent>(1024);
    let (mergeability_queue_tx, mergeability_queue_rx) = create_mergeability_queue();

    let (merge_queue_tx, merge_queue_fut) = start_merge_queue(
        ctx.clone(),
        merge_queue_max_interval,
        mergeability_queue_tx.clone(),
    );

    let (build_queue_tx, build_queue_rx) = create_buid_queue();

    let senders = QueueSenders {
        merge_queue: merge_queue_tx.clone(),
        mergeability_queue: mergeability_queue_tx,
        build_queue: build_queue_tx,
    };
    let senders2 = senders.clone();

    let service = async move {
        // In tests, we shutdown these futures by dropping the channel sender,
        // In that case, we need to wait until both of these futures resolve,
        // to make sure that they are able to handle all the events in the queue
        // before finishing.
        #[cfg(test)]
        {
            let _ = tokio::join!(
                consume_repository_events(ctx.clone(), repository_rx, senders2.clone()),
                consume_global_events(ctx.clone(), global_rx, senders2, gh_client, team_api),
                consume_mergeability_queue_events(ctx.clone(), mergeability_queue_rx),
                consume_build_queue_events(ctx.clone(), build_queue_rx, merge_queue_tx),
                merge_queue_fut
            );
        }
        // In real execution, the bot runs forever. If there is something that finishes
        // the futures early, it's essentially a bug.
        // FIXME: maybe we could just use join for both versions of the code.
        #[cfg(not(test))]
        {
            tokio::select! {
                _ = consume_repository_events(ctx.clone(), repository_rx, senders2.clone()) => {
                    tracing::error!("Repository event handling process has ended");
                }
                _ = consume_global_events(ctx.clone(), global_rx, senders2, gh_client, team_api) => {
                    tracing::error!("Global event handling process has ended");
                }
                _ = consume_mergeability_queue_events(ctx.clone(), mergeability_queue_rx) => {
                    tracing::error!("Mergeability queue handling process has ended")
                }
                _ = consume_build_queue_events(ctx.clone(), build_queue_rx, merge_queue_tx) => {
                    tracing::error!("Build queue handling process has ended")
                }
                _ = merge_queue_fut => {
                    tracing::error!("Merge queue handling process has ended");
                }
            }
        }
    };

    BorsProcess {
        repository_tx,
        global_tx,
        senders,
        bors_process: Box::pin(service),
    }
}

#[derive(Clone)]
pub struct QueueSenders {
    mergeability_queue: MergeabilityQueueSender,
    merge_queue: MergeQueueSender,
    build_queue: BuildQueueSender,
}

impl QueueSenders {
    pub fn merge_queue(&self) -> &MergeQueueSender {
        &self.merge_queue
    }
    pub fn mergeability_queue(&self) -> &MergeabilityQueueSender {
        &self.mergeability_queue
    }
    pub fn build_queue(&self) -> &BuildQueueSender {
        &self.build_queue
    }
}

async fn consume_repository_events(
    ctx: Arc<BorsContext>,
    mut repository_rx: mpsc::Receiver<BorsRepositoryEvent>,
    senders: QueueSenders,
) {
    while let Some(event) = repository_rx.recv().await {
        let ctx = ctx.clone();

        let span = tracing::info_span!("RepositoryEvent");
        tracing::debug!("Received repository event: {event:?}");
        if let Err(error) = handle_bors_repository_event(event, ctx, senders.clone())
            .instrument(span.clone())
            .await
        {
            handle_root_error(span, error);
        }

        #[cfg(test)]
        super::WAIT_FOR_WEBHOOK_COMPLETED.mark();
    }
}

async fn consume_global_events(
    ctx: Arc<BorsContext>,
    mut global_rx: mpsc::Receiver<BorsGlobalEvent>,
    senders: QueueSenders,
    gh_client: Octocrab,
    team_api: TeamApiClient,
) {
    while let Some(event) = global_rx.recv().await {
        let span = tracing::info_span!("GlobalEvent");
        if let Err(error) =
            handle_bors_global_event(event, ctx.clone(), &gh_client, &team_api, senders.clone())
                .instrument(span.clone())
                .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_mergeability_queue_events(
    ctx: Arc<BorsContext>,
    mergeability_queue_rx: MergeabilityQueueReceiver,
) {
    // We do not receive the sender here, but rather take it from dequeue every time.
    // Because when the last sender disappears, we want to end this loop, but that wouldn't work
    // if we also had a sender in this function.
    while let Some((mq_item, mq_tx)) = mergeability_queue_rx.dequeue().await {
        let ctx = ctx.clone();

        let span = tracing::debug_span!(
            "Mergeability check",
            item = ?mq_item
        );
        if let Err(error) = check_mergeability(ctx, mq_tx, mq_item)
            .instrument(span.clone())
            .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_build_queue_events(
    ctx: Arc<BorsContext>,
    mut build_queue_rx: BuildQueueReceiver,
    merge_queue_tx: MergeQueueSender,
) {
    while let Some(event) = build_queue_rx.recv().await {
        let ctx = ctx.clone();

        let span = tracing::debug_span!("Build queue event", "{event:?}");
        if let Err(error) = handle_build_queue_event(ctx, event, merge_queue_tx.clone())
            .instrument(span.clone())
            .await
        {
            handle_root_error(span, error);
        }

        #[cfg(test)]
        crate::bors::WAIT_FOR_BUILD_QUEUE.mark();
    }
}

#[allow(unused_variables)]
fn handle_root_error(span: Span, error: Error) {
    // In tests, we want to panic on all errors.
    #[cfg(test)]
    {
        panic!("Handler failed: {error:?}");
    }
    #[cfg(not(test))]
    {
        use crate::utils::logging::LogError;
        span.log_error(error);
    }
}
