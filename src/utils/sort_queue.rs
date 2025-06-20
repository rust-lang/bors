use crate::bors::RollupMode;
use crate::database::{BuildStatus, MergeableState, PullRequestModel};

/// Sorts pull requests according to merge queue priority rules.
///
/// Ordered by build status > mergeability > approval >
/// priority value > rollup > age.
pub fn sort_queue_prs(mut prs: Vec<PullRequestModel>) -> Vec<PullRequestModel> {
    prs.sort_by(|a, b| {
        // 1. Compare build status (lower value = higher priority)
        get_status_priority(a)
            .cmp(&get_status_priority(b))
            // 2. Compare mergeable state (0 = mergeable, 1 = conflicts/unknown)
            .then_with(|| get_mergeable_priority(a).cmp(&get_mergeable_priority(b)))
            // 3. Compare approval status (approved PRs should come first)
            .then_with(|| a.is_approved().cmp(&b.is_approved()).reverse())
            // 4. Compare priority numbers (higher priority should come first)
            .then_with(|| b.priority.unwrap_or(0).cmp(&a.priority.unwrap_or(0)))
            // 5. Compare rollup mode (-1 = never/iffy, 0 = maybe, 1 = always)
            .then_with(|| {
                get_rollup_priority(a.rollup.as_ref()).cmp(&get_rollup_priority(b.rollup.as_ref()))
            })
            // 6. Compare PR numbers (older first)
            .then_with(|| a.number.cmp(&b.number))
    });
    prs
}

fn get_status_priority(pr: &PullRequestModel) -> i32 {
    match &pr.auto_build {
        Some(build) => match build.status {
            BuildStatus::Pending => 1,
            BuildStatus::Success => 6,
            BuildStatus::Failure => 5,
            BuildStatus::Cancelled | BuildStatus::Timeouted => 4,
        },
        None => {
            if pr.is_approved() {
                2 // approved but no build
            } else {
                3 // no status
            }
        }
    }
}

fn get_mergeable_priority(pr: &PullRequestModel) -> i32 {
    match pr.mergeable_state {
        MergeableState::Mergeable => 0,
        MergeableState::HasConflicts => 1,
        MergeableState::Unknown => 1,
    }
}

fn get_rollup_priority(rollup: Option<&RollupMode>) -> i32 {
    match rollup {
        Some(RollupMode::Always) => 1,
        Some(RollupMode::Maybe) | None => 0,
        Some(RollupMode::Iffy) | Some(RollupMode::Never) => -1,
    }
}
