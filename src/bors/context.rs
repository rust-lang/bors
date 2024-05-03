use crate::{bors::command::CommandParser, database::DbClient, github::GithubRepoName};
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
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub async fn new(
        parser: CommandParser,
        db: Arc<dyn DbClient>,
        global_client: Arc<dyn RepositoryLoader<Client>>,
    ) -> anyhow::Result<Self> {
        let repositories = global_client.load_repositories().await?;
        let repositories = RwLock::new(repositories);
        Ok(Self {
            parser,
            db,
            repository_loader: global_client,
            repositories,
        })
    }
}
