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

/*
#[derive(Debug, PartialEq, Clone)]
#[allow(dead_code)]
pub enum PrStatus {
    Pending,
    Approved,
    Empty,
    Error,
    Failure,
    Success,
}

#[allow(dead_code)]
impl PrStatus {
    fn priority(&self) -> i32 {
        match self {
            PrStatus::Pending => 1,
            PrStatus::Approved => 2,
            PrStatus::Empty => 3,
            PrStatus::Error => 4,
            PrStatus::Failure => 5,
            PrStatus::Success => 6,
        }
    }
}

#[allow(dead_code)]
fn determine_pr_status(pr: &PullRequestModel) -> PrStatus {
    if let Some(build) = &pr.try_build {
        match build.status {
            BuildStatus::Pending => PrStatus::Pending,
            BuildStatus::Success => PrStatus::Success,
            BuildStatus::Failure => PrStatus::Failure,
            BuildStatus::Cancelled => PrStatus::Error,
            BuildStatus::Timeouted => PrStatus::Error,
        }
    } else if pr.approved_by.is_some() {
        PrStatus::Approved
    } else {
        PrStatus::Empty
    }
}

/// Starts a fresh build for a pull request
#[allow(dead_code)]
async fn start_build(
    state: &PullRequestModel,
    repo: &Arc<RepositoryState>,
    db: &PgDbClient,
) -> Result<bool> {
    let pr = repo.client.get_pull_request(state.number).await?;
    let author = GithubUser {
        id: UserId(0), // System action
        username: "bors".to_string(),
        html_url: Url::parse("https://github.com/bors").unwrap(),
    };
    command_try_build(
        repo.clone(),
        Arc::new(db.clone()),
        &pr,
        &author,
        None,
        vec![],
    )
    .await?;

    Ok(true)
}

/// Attempts to rebuild a pull request, falling back to fresh build if needed
#[allow(dead_code)]
async fn start_build_or_rebuild(
    state: &PullRequestModel,
    repo: &Arc<RepositoryState>,
    db: &PgDbClient,
) -> Result<bool> {
    let pr = repo.client.get_pull_request(state.number).await?;
    let author = GithubUser {
        id: UserId(0), // System action
        username: "bors".to_string(),
        html_url: Url::parse("https://github.com/bors").unwrap(),
    };

    // First try to rebuild if there's a previous build
    if let Some(prev_build) = &state.try_build {
        if prev_build.status == BuildStatus::Failure {
            command_try_build(
                repo.clone(),
                Arc::new(db.clone()),
                &pr,
                &author,
                Some(Parent::CommitSha(CommitSha(prev_build.parent.clone()))),
                vec![],
            )
            .await?;
            return Ok(true);
        }
    }

    start_build(state, repo, db).await
}
*/
