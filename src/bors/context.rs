use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use derive_builder::Builder;
use octocrab::Octocrab;

use crate::{bors::command::CommandParser, github::GithubRepoName, PgDbClient, TeamApiClient};

use super::RepositoryState;

#[derive(Builder)]
pub struct BorsContext {
    pub parser: CommandParser,
    pub db: Arc<PgDbClient>,
    #[builder(field(
        ty = "HashMap<GithubRepoName, Arc<RepositoryState>>",
        build = "RwLock::new(self.repositories.clone())"
    ))]
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState>>>,
    pub gh_client: Octocrab,
    #[builder(default)]
    pub ci_client: Option<Octocrab>,
    pub ci_repo_map: HashMap<GithubRepoName, GithubRepoName>,
    pub team_api_client: TeamApiClient,
}
