use arc_swap::ArcSwap;

use crate::{bors::command::CommandParser, database::DbClient, github::GithubRepoName};
use std::{collections::HashMap, sync::Arc};

use super::{GlobalClient, RepositoryClient, RepositoryState};

pub struct BorsContext<Client: RepositoryClient> {
    pub parser: CommandParser,
    pub db: Arc<dyn DbClient>,
    pub global_client: Arc<dyn GlobalClient<Client>>,
    pub repositories: Arc<ArcSwap<HashMap<GithubRepoName, Arc<RepositoryState<Client>>>>>,
}

impl<Client: RepositoryClient> BorsContext<Client> {
    pub async fn new(
        parser: CommandParser,
        db: Arc<dyn DbClient>,
        global_client: Arc<dyn GlobalClient<Client>>,
    ) -> Self {
        // this unwrap is making me nervous, but if lhe repos loading
        // fails we might as well restart the bot
        let repositories = global_client.load_repositories().await.unwrap();
        let repositories = Arc::new(ArcSwap::new(Arc::new(repositories)));
        Self {
            parser,
            db,
            global_client,
            repositories,
        }
    }
}
