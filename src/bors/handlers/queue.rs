use crate::bors::RepositoryState;
use crate::database::{PgDbClient, PullRequestModel, TreeState};
use crate::github::GithubRepoName;
use anyhow::Result;
use std::sync::Arc;

use tracing::debug;

/// Process the queue of pull requests for each repository
#[allow(dead_code)]
pub async fn process_queue(
    states: &[(GithubRepoName, Vec<PullRequestModel>)],
    repos: &[(GithubRepoName, Arc<RepositoryState>)],
    db: Arc<PgDbClient>,
) -> Result<()> {
    for (repo_label, _repo_states) in states {
        // Get repository state
        let repo = repos
            .iter()
            .find(|(name, _)| name == repo_label)
            .map(|(_, repo)| repo)
            .ok_or_else(|| anyhow::anyhow!("Repository not found"))?;

        debug!("process_queue: checking tree state for {}", repo_label);
        let (tree_state, _) = blocked_by_closed_tree(repo, None, &db).await?;
        debug!(
            "process_queue: tree state for {} is {:?}",
            repo_label, tree_state
        );
    }
    Ok(())
}

/// Check if a PR is blocked by a closed tree
pub async fn blocked_by_closed_tree(
    repo: &RepositoryState,
    priority: Option<i32>,
    db: &PgDbClient,
) -> Result<(Option<TreeState>, Option<String>)> {
    let repo_models = db.get_repository_treeclosed(repo.repository()).await?;
    let repo_model = repo_models
        .first()
        .ok_or_else(|| anyhow::anyhow!("No repository model found"))?;

    Ok((
        match repo_model.treeclosed {
            TreeState::Open => None,
            TreeState::Closed(threshold) => {
                if priority.unwrap_or(0) < threshold as i32 {
                    Some(repo_model.treeclosed)
                } else {
                    None
                }
            }
        },
        repo_model.treeclosed_src.clone(),
    ))
}
