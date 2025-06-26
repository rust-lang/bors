use std::sync::Arc;

use crate::{
    BorsContext, bors::RepositoryState, database::BuildStatus, utils::sort_queue::sort_queue_prs,
};

pub type MergeQueueEvent = ();

pub async fn handle_merge_queue(ctx: Arc<BorsContext>) -> anyhow::Result<()> {
    let repos: Vec<Arc<RepositoryState>> =
        ctx.repositories.read().unwrap().values().cloned().collect();

    for repo in repos {
        let repo_name = repo.repository();
        let repo_db = match ctx.db.repo_db(repo_name).await? {
            Some(repo) => repo,
            None => {
                tracing::error!("Repository {repo_name} not found");
                continue;
            }
        };
        let priority = repo_db.tree_state.priority();
        let prs = ctx.db.get_merge_queue_prs(repo_name, priority).await?;

        // Sort PRs according to merge queue priority rules.
        // Pending PRs come first - this is important as we make sure to block the queue to
        // prevent starting simultaneous auto-builds.
        let prs = sort_queue_prs(prs);

        for pr in prs {
            let pr_num = pr.number;

            if let Some(auto_build) = &pr.auto_build {
                match auto_build.status {
                    // Build in progress - stop queue. We can only have one PR built at a time.
                    BuildStatus::Pending => {
                        tracing::info!("PR {pr_num} has a pending build - blocking queue");
                        break;
                    }
                    // Build successful - point the base branch to the merged commit.
                    BuildStatus::Success => {
                        todo!("Merge to base branch");
                    }
                    BuildStatus::Failure | BuildStatus::Cancelled | BuildStatus::Timeouted => {
                        unreachable!("Failed auto builds should be filtered out by SQL query");
                    }
                }
            }
        }
    }

    Ok(())
}
