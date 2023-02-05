mod config;
pub mod operations;

use std::collections::HashMap;

use crate::github::api::config::{RepositoryConfig, CONFIG_FILE_PATH};
use crate::github::event::PullRequestComment;
use anyhow::Context;
use base64::Engine;
use octocrab::models::{App, AppId, InstallationRepositories, Repository};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretVec};

use crate::github::GithubRepoName;

/// Provides access to managed GitHub repositories.
pub struct GithubAppClient {
    app: App,
    client: Octocrab,
    repositories: HashMap<GithubRepoName, RepositoryClient>,
}

impl GithubAppClient {
    /// Loads repositories managed by the Bors GitHub app with the given ID.
    pub async fn load(
        app_id: AppId,
        private_key: SecretVec<u8>,
    ) -> anyhow::Result<GithubAppClient> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.expose_secret().as_ref())
            .context("Could not encode private key")?;

        let client = Octocrab::builder()
            .app(app_id, key)
            .build()
            .context("Could not create octocrab builder")?;

        let app = client
            .current()
            .app()
            .await
            .context("Could not load Github App")?;

        let repositories = load_repositories(&client).await?;
        Ok(GithubAppClient {
            app,
            client,
            repositories,
        })
    }

    /// Re-download information about repositories connected to this GitHub app.
    pub async fn reload_repositories(&mut self) -> anyhow::Result<()> {
        self.repositories = load_repositories(&self.client).await?;
        Ok(())
    }

    pub fn get_repo_client(&self, key: &GithubRepoName) -> Option<&RepositoryClient> {
        self.repositories.get(key)
    }

    /// Returns true if the comment was made by this bot.
    pub fn is_comment_internal(&self, comment: &PullRequestComment) -> bool {
        comment.author.html_url == self.app.html_url
    }
}

/// Provides access to a single app installation (repository).
#[derive(Debug)]
pub struct RepositoryClient {
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    client: Octocrab,
    // We store the name separately, because repository has an optional owner, but at this point
    // we must always have some owner of the repo.
    repo_name: GithubRepoName,
    repository: Repository,
    config: RepositoryConfig,
}

impl RepositoryClient {
    pub fn client(&self) -> &Octocrab {
        &self.client
    }

    pub fn name(&self) -> &GithubRepoName {
        &self.repo_name
    }

    pub fn repository(&self) -> &Repository {
        &self.repository
    }
}

/// Loads repositories that are connected to the given GitHub App client.
pub async fn load_repositories(
    client: &Octocrab,
) -> anyhow::Result<HashMap<GithubRepoName, RepositoryClient>> {
    let installations = client
        .apps()
        .installations()
        .send()
        .await
        .context("Could not load app installations")?;

    let mut repositories = HashMap::default();
    for installation in installations {
        if let Some(ref repositories_url) = installation.repositories_url {
            let installation_client = client.installation(installation.id);

            match installation_client
                .get::<InstallationRepositories, _, ()>(repositories_url, None)
                .await
            {
                Ok(repos) => {
                    for repo in repos.repositories {
                        let Some(repo_client) = load_repository(installation_client.clone(), repo).await else { continue; };
                        log::info!("Loaded repository {}", repo_client.name());

                        if let Some(existing) =
                            repositories.insert(repo_client.name().clone(), repo_client)
                        {
                            return Err(anyhow::anyhow!(
                                "Repository {} found in multiple installations!",
                                existing.repo_name
                            ));
                        }
                    }
                }
                Err(error) => {
                    log::error!(
                        "Could not load repositories of installation {}: {error}",
                        installation.id
                    );
                }
            };
        }
    }
    Ok(repositories)
}

async fn load_repository(repo_client: Octocrab, repo: Repository) -> Option<RepositoryClient> {
    let Some(owner) = repo.owner.clone() else {
        return None;
    };

    let name = GithubRepoName::new(&owner.login, &repo.name);
    log::info!("Found repository {name}");

    let config = match load_repository_config(&repo_client, &name).await {
        Ok(config) => {
            log::debug!("Loaded repository config for {name}: {config:#?}");
            config
        }
        Err(error) => {
            log::error!("Could not load repository config for {name}: {error:?}");
            return None;
        }
    };
    Some(RepositoryClient {
        client: repo_client,
        repo_name: name,
        repository: repo,
        config,
    })
}

/// Loads repository configuration from a file located at `[CONFIG_FILE_PATH]` in the main
/// branch.
async fn load_repository_config(
    gh_client: &Octocrab,
    repo: &GithubRepoName,
) -> anyhow::Result<RepositoryConfig> {
    let mut response = gh_client
        .repos(&repo.owner, &repo.name)
        .get_content()
        .path(CONFIG_FILE_PATH)
        .send()
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not fetch {CONFIG_FILE_PATH} from {repo}: {error:?}")
        })?;

    let engine = base64::engine::general_purpose::STANDARD;
    response
        .take_items()
        .into_iter()
        .next()
        .and_then(|content| content.content)
        .ok_or_else(|| anyhow::anyhow!("Configuration file not found"))
        .and_then(|content| Ok(engine.decode(content.trim())?))
        .and_then(|content| Ok(String::from_utf8(content)?))
        .and_then(|content| {
            let config: RepositoryConfig = toml::from_str(&content).map_err(|error| {
                anyhow::anyhow!("Could not deserialize repository config: {error:?}")
            })?;
            Ok(config)
        })
}
