use arc_swap::ArcSwap;

use crate::{bors::command::CommandParser, database::DbClient, github::GithubRepoName};
use std::{collections::HashMap, sync::Arc};

use super::{RepositoryClient, RepositoryLoader, RepositoryState};

pub struct BorsContext<Client: RepositoryClient> {
    pub parser: CommandParser,
    pub db: Arc<dyn DbClient>,
    pub repository_loader: Arc<dyn RepositoryLoader<Client>>,
    pub repositories: Arc<ArcSwap<HashMap<GithubRepoName, Arc<RepositoryState<Client>>>>>,
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub async fn new(
        parser: CommandParser,
        db: Arc<dyn DbClient>,
        global_client: Arc<dyn RepositoryLoader<Client>>,
    ) -> anyhow::Result<Self> {
        // this unwrap is making me nervous, but if lhe repos loading
        // fails we might as well restart the bot
        let repositories = global_client.load_repositories().await?;
        let repositories = Arc::new(ArcSwap::new(Arc::new(repositories)));
        Ok(Self {
            parser,
            db,
            repository_loader: global_client,
            repositories,
        })
    }
}
