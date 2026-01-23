use crate::bors::RollupMode;
use crate::database::{MergeableState, PullRequestModel, QueueStatus};

/// Sorts pull requests according to merge queue priority rules.
/// Ordered by: ready for merge > pending builds > approved > failed > not approved > mergeability
/// > priority > rollup > age.
pub fn sort_queue_prs(mut prs: Vec<PullRequestModel>) -> Vec<PullRequestModel> {
    prs.sort_by(|a, b| {
        // 1. Compare queue status (ready for merge > pending > approved > failed > not approved)
        get_queue_status_priority(&a.queue_status())
            .cmp(&get_queue_status_priority(&b.queue_status()))
            // 2. Compare mergeability state (0 = mergeable/unknown, 1 = conflicts)
            .then_with(|| get_mergeable_priority(a).cmp(&get_mergeable_priority(b)))
            // 3. Compare priority numbers (higher priority should come first)
            .then_with(|| {
                a.priority
                    .unwrap_or(0)
                    .cmp(&b.priority.unwrap_or(0))
                    .reverse()
            })
            // 4. Compare rollup mode (always > maybe > iffy > never)
            .then_with(|| {
                get_rollup_priority(a.rollup.as_ref()).cmp(&get_rollup_priority(b.rollup.as_ref()))
            })
            // 5. Compare PR numbers (older first)
            .then_with(|| a.number.cmp(&b.number))
    });
    prs
}

fn get_queue_status_priority(status: &QueueStatus) -> u32 {
    match status {
        QueueStatus::ReadyForMerge(_, _) => 0,
        QueueStatus::Pending(_, _) => 1,
        QueueStatus::Approved(_) => 2,
        QueueStatus::Failed(_, _) => 3,
        QueueStatus::NotApproved | QueueStatus::NotOpen => 4,
    }
}

fn get_mergeable_priority(pr: &PullRequestModel) -> u32 {
    match pr.mergeable_status() {
        MergeableState::Mergeable | MergeableState::Unknown => 0,
        MergeableState::HasConflicts => 1,
    }
}

fn get_rollup_priority(rollup: Option<&RollupMode>) -> u32 {
    match rollup {
        Some(RollupMode::Always) => 3,
        Some(RollupMode::Maybe) => 2,
        None => 2, // Default case
        Some(RollupMode::Iffy) => 1,
        Some(RollupMode::Never) => 0,
    }
}
