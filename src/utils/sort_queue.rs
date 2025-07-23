use crate::bors::RollupMode;
use crate::database::{BuildStatus, MergeableState, PullRequestModel};

/// Sorts pull requests according to merge queue priority rules.
/// Ordered by pending builds > success builds > approval > mergeability > priority value > rollup > age.
pub fn sort_queue_prs(mut prs: Vec<PullRequestModel>) -> Vec<PullRequestModel> {
    prs.sort_by(|a, b| {
        // 1. Pending builds come first (to block merge queue)
        get_queue_blocking_priority(a)
            .cmp(&get_queue_blocking_priority(b))
            // 2. Compare approval status (approved PRs should come first)
            .then_with(|| a.is_approved().cmp(&b.is_approved()).reverse())
            // 3. Compare build status within approval groups
            .then_with(|| get_status_priority(a).cmp(&get_status_priority(b)))
            // 4. Compare mergeable state (0 = mergeable, 1 = conflicts/unknown)
            .then_with(|| get_mergeable_priority(a).cmp(&get_mergeable_priority(b)))
            // 5. Compare priority numbers (higher priority should come first)
            .then_with(|| {
                a.priority
                    .unwrap_or(0)
                    .cmp(&b.priority.unwrap_or(0))
                    .reverse()
            })
            // 6. Compare rollup mode (-1 = never/iffy, 0 = maybe, 1 = always)
            .then_with(|| {
                get_rollup_priority(a.rollup.as_ref()).cmp(&get_rollup_priority(b.rollup.as_ref()))
            })
            // 7. Compare PR numbers (older first)
            .then_with(|| a.number.cmp(&b.number))
    });
    prs
}

fn get_queue_blocking_priority(pr: &PullRequestModel) -> u32 {
    match &pr.auto_build {
        Some(build) => match build.status {
            // Pending builds must come first to block the merge queue
            BuildStatus::Pending => 0,
            // All other statuses come after
            _ => 1,
        },
        None => 1, // No build - can potentially start new build
    }
}

fn get_status_priority(pr: &PullRequestModel) -> u32 {
    match &pr.auto_build {
        Some(build) => match build.status {
            BuildStatus::Success => 0,
            BuildStatus::Pending => 1,
            BuildStatus::Failure => 3,
            BuildStatus::Cancelled | BuildStatus::Timeouted => 2,
        },
        None => {
            if pr.is_approved() {
                1 // Approved but no build - should be prioritized
            } else {
                2 // No status
            }
        }
    }
}

fn get_mergeable_priority(pr: &PullRequestModel) -> u32 {
    match pr.mergeable_state {
        MergeableState::Mergeable => 0,
        MergeableState::HasConflicts => 1,
        MergeableState::Unknown => 1,
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
