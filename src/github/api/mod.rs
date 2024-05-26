use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use octocrab::models::{App, AppId, InstallationRepositories, Repository};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretVec};

use client::GithubRepositoryClient;

use crate::bors::{RepositoryClient, RepositoryState};
use crate::github::GithubRepoName;
use crate::permissions::TeamApiClient;

pub mod client;
pub(crate) mod operations;

type GithubRepositoryState = RepositoryState<GithubRepositoryClient>;

type GithubRepositoryMap = HashMap<GithubRepoName, Arc<GithubRepositoryState>>;

fn base_github_html_url() -> &'static str {
    "https://github.com"
}

pub fn create_github_client(app_id: AppId, private_key: SecretVec<u8>) -> anyhow::Result<Octocrab> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.expose_secret().as_ref())
        .context("Could not encode private key")?;

    Octocrab::builder()
        .app(app_id, key)
        .build()
        .context("Could not create octocrab builder")
}

/// Loads repositories that are connected to the given GitHub App client.
pub async fn load_repositories(
    client: &Octocrab,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<GithubRepositoryMap> {
    let installations = client
        .apps()
        .installations()
        .send()
        .await
        .context("Could not load app installations")?;

    // installation client can not be used to load current app
    // https://docs.github.com/en/rest/apps/apps?apiVersion=2022-11-28#get-the-authenticated-app
    let app = client
        .current()
        .app()
        .await
        .context("Could not load Github App")?;

    let mut repositories = HashMap::default();
    for installation in installations {
        let installation_client = client.installation(installation.id);

        let repos = match load_installation_repos(&installation_client).await {
            Ok(repos) => repos,
            Err(error) => {
                tracing::error!(
                    "Could not load repositories of installation {}: {error:?}",
                    installation.id
                );
                continue;
            }
        };
        for repo in repos {
            match create_repo_state(
                app.clone(),
                installation_client.clone(),
                team_api_client,
                repo.clone(),
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("Cannot load repository {:?}: {error:?}", repo.full_name)
            }) {
                Ok(repo_state) => {
                    tracing::info!("Loaded repository {}", repo_state.repository);

                    if let Some(existing) =
                        repositories.insert(repo_state.repository.clone(), Arc::new(repo_state))
                    {
                        return Err(anyhow::anyhow!(
                            "Repository {} found in multiple installations!",
                            existing.repository
                        ));
                    }
                }
                Err(error) => {
                    tracing::error!(
                        "Could not load repository {}: {error:?}",
                        repo.full_name.unwrap_or_default()
                    );
                }
            }
        }
    }
    Ok(repositories)
}

/// Load all repositories of a single GitHub app installation.
/// The installation endpoint uses a weird pagination API, so we cannot use octocrab::Page directly.
async fn load_installation_repos(client: &Octocrab) -> anyhow::Result<Vec<Repository>> {
    let first_page = client
        .get::<InstallationRepositories, _, _>(
            "/installation/repositories?per_page=100",
            None::<&()>,
        )
        .await?;
    let mut repos = first_page.repositories;
    let mut page = 2;
    while repos.len() < first_page.total_count as usize {
        repos.extend(
            client
                .get::<InstallationRepositories, _, _>(
                    format!("/installation/repositories?per_page=100&page={page}"),
                    None::<&()>,
                )
                .await?
                .repositories,
        );
        page += 1;
    }
    Ok(repos)
}

async fn create_repo_state(
    app: App,
    repo_client: Octocrab,
    team_api_client: &TeamApiClient,
    repo: Repository,
) -> anyhow::Result<GithubRepositoryState> {
    let Some(owner) = repo.owner.clone() else {
        return Err(anyhow::anyhow!("Repository {} has no owner", repo.name));
    };

    let name = GithubRepoName::new(&owner.login, &repo.name);
    tracing::info!("Found repository {name}");

    let client = GithubRepositoryClient {
        app,
        client: repo_client,
        repo_name: name.clone(),
        repository: repo,
    };

    let permissions = team_api_client
        .load_permissions(&name)
        .await
        .context("Could not load permissions for repository {name}")?;

    let config = match client.load_config().await {
        Ok(config) => {
            tracing::info!("Loaded repository config for {name}: {config:#?}");
            config
        }
        Err(error) => {
            return Err(anyhow::anyhow!(
                "Could not load repository config for {name}: {error:?}"
            ));
        }
    };

    Ok(RepositoryState {
        repository: name,
        client,
        config: ArcSwap::new(Arc::new(config)),
        permissions: ArcSwap::new(Arc::new(permissions)),
    })
}
