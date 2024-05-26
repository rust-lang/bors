use crate::{
    bors::command::CommandParser, database::DbClient, github::GithubRepoName,
    permissions::TeamApiClient,
};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use super::{RepositoryClient, RepositoryLoader, RepositoryState};

pub struct BorsContext<Client: RepositoryClient> {
    pub parser: CommandParser,
    pub db: Arc<dyn DbClient>,
    pub repository_loader: Arc<dyn RepositoryLoader<Client>>,
    pub repositories: RwLock<HashMap<GithubRepoName, Arc<RepositoryState<Client>>>>,
    pub team_api_client: TeamApiClient,
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub async fn new(
        parser: CommandParser,
        db: Arc<dyn DbClient>,
        repository_loader: Arc<dyn RepositoryLoader<Client>>,
        team_api_client: TeamApiClient,
    ) -> anyhow::Result<Self> {
        let repositories = repository_loader
            .load_repositories(&team_api_client)
            .await?;
        let repositories = RwLock::new(repositories);
        Ok(Self {
            parser,
            db,
            repository_loader,
            repositories,
            team_api_client,
        })
    }
}
