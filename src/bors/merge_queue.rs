use std::sync::Arc;

use crate::BorsContext;

pub type MergeQueueEvent = ();

pub async fn handle_merge_queue(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    Ok(())
}
