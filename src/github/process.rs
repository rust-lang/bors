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
    pub merge_queue_tx: MergeQueueSender,
    pub mergeability_queue_tx: MergeabilityQueueSender,
    pub bors_process: Pin<Box<dyn Future<Output = ()> + Send>>,
}

/// Creates a future with a Bors process that continuously receives webhook events and reacts to
/// them.
pub fn create_bors_process(
    ctx: BorsContext,
    gh_client: Octocrab,
    team_api: TeamApiClient,
    merge_queue_max_interval: chrono::Duration,
) -> BorsProcess {
    let (repository_tx, repository_rx) = mpsc::channel::<BorsRepositoryEvent>(1024);
    let (global_tx, global_rx) = mpsc::channel::<BorsGlobalEvent>(1024);
    let (mergeability_queue_tx, mergeability_queue_rx) = create_mergeability_queue();
    let mergeability_queue_tx2 = mergeability_queue_tx.clone();

    let ctx = Arc::new(ctx);

    let (merge_queue_tx, merge_queue_fut) =
        start_merge_queue(ctx.clone(), merge_queue_max_interval);
    let merge_queue_tx2 = merge_queue_tx.clone();

    let service = async move {
        // In tests, we shutdown these futures by dropping the channel sender,
        // In that case, we need to wait until both of these futures resolve,
        // to make sure that they are able to handle all the events in the queue
        // before finishing.
        #[cfg(test)]
        {
            let _ = tokio::join!(
                consume_repository_events(
                    ctx.clone(),
                    repository_rx,
                    mergeability_queue_tx2.clone(),
                    merge_queue_tx2.clone()
                ),
                consume_global_events(
                    ctx.clone(),
                    global_rx,
                    mergeability_queue_tx2,
                    merge_queue_tx2,
                    gh_client,
                    team_api
                ),
                consume_mergeability_queue(ctx.clone(), mergeability_queue_rx),
                merge_queue_fut
            );
        }
        // In real execution, the bot runs forever. If there is something that finishes
        // the futures early, it's essentially a bug.
        // FIXME: maybe we could just use join for both versions of the code.
        #[cfg(not(test))]
        {
            tokio::select! {
                _ = consume_repository_events(ctx.clone(), repository_rx, mergeability_queue_tx2.clone(), merge_queue_tx2.clone()) => {
                    tracing::error!("Repository event handling process has ended");
                }
                _ = consume_global_events(ctx.clone(), global_rx, mergeability_queue_tx2, merge_queue_tx2, gh_client, team_api) => {
                    tracing::error!("Global event handling process has ended");
                }
                _ = consume_mergeability_queue(ctx.clone(), mergeability_queue_rx) => {
                    tracing::error!("Mergeability queue handling process has ended")
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
        mergeability_queue_tx,
        merge_queue_tx,
        bors_process: Box::pin(service),
    }
}

async fn consume_repository_events(
    ctx: Arc<BorsContext>,
    mut repository_rx: mpsc::Receiver<BorsRepositoryEvent>,
    mergeability_queue_tx: MergeabilityQueueSender,
    merge_queue_tx: MergeQueueSender,
) {
    while let Some(event) = repository_rx.recv().await {
        let ctx = ctx.clone();
        let mergeability_queue_tx = mergeability_queue_tx.clone();

        let span = tracing::info_span!("RepositoryEvent");
        tracing::debug!("Received repository event: {event:?}");
        if let Err(error) =
            handle_bors_repository_event(event, ctx, mergeability_queue_tx, merge_queue_tx.clone())
                .instrument(span.clone())
                .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_global_events(
    ctx: Arc<BorsContext>,
    mut global_rx: mpsc::Receiver<BorsGlobalEvent>,
    mergeability_queue_tx: MergeabilityQueueSender,
    merge_queue_tx: MergeQueueSender,
    gh_client: Octocrab,
    team_api: TeamApiClient,
) {
    while let Some(event) = global_rx.recv().await {
        let ctx = ctx.clone();
        let mergeability_queue_tx = mergeability_queue_tx.clone();
        let merge_queue_tx = merge_queue_tx.clone();

        let span = tracing::info_span!("GlobalEvent");
        tracing::trace!("Received global event: {event:?}");
        if let Err(error) = handle_bors_global_event(
            event,
            ctx,
            &gh_client,
            &team_api,
            mergeability_queue_tx,
            merge_queue_tx,
        )
        .instrument(span.clone())
        .await
        {
            handle_root_error(span, error);
        }
    }
}

async fn consume_mergeability_queue(
    ctx: Arc<BorsContext>,
    mergeability_queue_rx: MergeabilityQueueReceiver,
) {
    while let Some((mq_item, mq_tx)) = mergeability_queue_rx.dequeue().await {
        let ctx = ctx.clone();

        let span = tracing::debug_span!(
            "Mergeability check",
            repo = mq_item.pull_request.repo.to_string(),
            pr = mq_item.pull_request.pr_number.0,
            attempt = mq_item.attempt
        );
        if let Err(error) = check_mergeability(ctx, mq_tx, mq_item)
            .instrument(span.clone())
            .await
        {
            handle_root_error(span, error);
        }
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
