use std::sync::Arc;
use anyhow::Result;
use crate::bors::RepositoryState;
use crate::database::{PgDbClient, TreeState, BuildStatus, PullRequestModel};
use crate::github::{GithubRepoName, GithubUser, CommitSha};
use crate::bors::handlers::trybuild::command_try_build;
use octocrab::models::UserId;
use url::Url;
use crate::bors::command::Parent;

use tracing::debug;

#[derive(Debug, PartialEq, Clone)]
pub enum PrStatus {
    Pending,
    Approved,
    Empty,
    Error,
    Failure,
    Success,
}

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

/// Process the queue of pull requests for each repository
pub async fn process_queue(
    states: &[(GithubRepoName, Vec<PullRequestModel>)],
    repos: &[(GithubRepoName, Arc<RepositoryState>)],
    db: Arc<PgDbClient>,
) -> Result<()> {
    for (repo_label, repo_states) in states {
        // Get repository state
        let repo = repos.iter()
            .find(|(name, _)| name == repo_label)
            .map(|(_, repo)| repo)
            .ok_or_else(|| anyhow::anyhow!("Repository not found"))?;

        // Sort states by priority
        let mut repo_states = repo_states.to_vec();
        repo_states.sort_by(|a, b| a.priority.cmp(&b.priority));

        for state in repo_states.iter() {
            debug!("process_queue: state={:?}, building {}", state, repo_label);
            
            let (tree_state, _) = blocked_by_closed_tree(repo, state.priority, &db).await?;
            if let Some(TreeState::Closed(_)) = tree_state {
                continue;
            }

            // Determine PR status
            let status = determine_pr_status(state);
            
            match status {
                PrStatus::Pending => {
                    if state.try_build.is_none() {
                        break;
                    }
                }
                PrStatus::Success => {
                    if state.try_build.is_some() {
                        break;
                    }
                }
                PrStatus::Empty => {
                    if state.approved_by.is_some() {
                        if start_build_or_rebuild(state, repo, &db).await? {
                            return Ok(());
                        }
                    }
                }
                PrStatus::Error | PrStatus::Failure => {}
                PrStatus::Approved => {}
            }
        }

        // Process try builds
        for state in repo_states.iter() {
            if state.try_build.is_some() && state.approved_by.is_none() {
                if start_build(state, repo, &db).await? {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

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

/// Check if a PR is blocked by a closed tree
pub async fn blocked_by_closed_tree(
    repo: &RepositoryState,
    priority: Option<i32>,
    db: &PgDbClient,
) -> Result<(Option<TreeState>, Option<String>)> {
    let repo_models = db.get_repository_treeclosed(repo.repository()).await?;
    let repo_model = repo_models.first().ok_or_else(|| anyhow::anyhow!("No repository model found"))?;
    
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
        repo_model.treeclosed_src.clone()
    ))
}

/// Starts a fresh build for a pull request
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
    ).await?;
    
    Ok(true)
}

/// Attempts to rebuild a pull request, falling back to fresh build if needed
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
            ).await?;
            return Ok(true);
        }
    }

    start_build(state, repo, db).await
}