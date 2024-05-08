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
        global_client: Arc<dyn RepositoryLoader<Client>>,
        team_api_client: TeamApiClient,
    ) -> anyhow::Result<Self> {
        let repositories = global_client.load_repositories().await?;
        for name in repositories.keys() {
            team_api_client.fetch_permissions(name).await?;
        }
        let repositories = RwLock::new(repositories);
        Ok(Self {
            parser,
            db,
            repository_loader: global_client,
            repositories,
            team_api_client,
        })
    }
}
