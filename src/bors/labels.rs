use itertools::Itertools;
use std::collections::HashSet;
use tracing::log;

use crate::bors::RepositoryState;
use crate::github::{LabelModification, LabelTrigger, PullRequest, PullRequestNumber};

/// If there are any label modifications that should be performed on the given PR when `trigger`
/// happens, this function will perform them.
pub async fn handle_label_trigger(
    repo: &RepositoryState,
    pr_number: PullRequestNumber,
    pr: &PullRequest,
    trigger: LabelTrigger,
) -> anyhow::Result<()> {
    let mut add: Vec<String> = Vec::new();
    let mut remove: Vec<String> = Vec::new();
    if let Some(operation) = repo.config.load().labels.get(&trigger) {
        log::debug!("Performing label operation {operation:?}");

        if !operation.should_apply_to(pr) {
            log::debug!("Skipping operation (pr labels={:?})", pr.labels);
            return Ok(());
        }

        (add, remove) = operation
            .modifications()
            .iter()
            .partition_map(|modification| match modification {
                LabelModification::Add(label) => itertools::Either::Left(label.clone()),
                LabelModification::Remove(label) => itertools::Either::Right(label.clone()),
            });

        // If we know the GitHub state, only remove/add labels that will actually have any effect on the
        // PR.
        let existing_labels: HashSet<&str> = pr.labels.iter().map(|s| s.as_str()).collect();
        add.retain(|l| !existing_labels.contains(l.as_str()));
        remove.retain(|l| existing_labels.contains(l.as_str()));
        log::info!(
            "Filtered labels: requested = {operation:?}, pr = {existing_labels:?}, add = {add:?}, remove = {remove:?}"
        );
    }

    if !add.is_empty() {
        log::info!("Adding label(s) {add:?}");
        repo.client.add_labels(pr_number, &add).await?;
    }
    if !remove.is_empty() {
        log::info!("Removing label(s) {remove:?}");
        repo.client.remove_labels(pr_number, &remove).await?;
    }
    Ok(())
}
