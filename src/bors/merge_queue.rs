use std::future::Future;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::Instrument;

use crate::BorsContext;

pub type MergeQueueEvent = ();

pub async fn merge_queue_tick(_ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    Ok(())
}

pub fn start_merge_queue(
    ctx: Arc<BorsContext>,
) -> (mpsc::Sender<MergeQueueEvent>, impl Future<Output = ()>) {
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
