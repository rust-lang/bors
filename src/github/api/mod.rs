use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use arc_swap::ArcSwap;
use octocrab::models::{App, AppId, InstallationRepositories, Repository};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretString};

use client::GithubRepositoryClient;

use crate::bors::RepositoryState;
use crate::config::RepositoryConfig;
use crate::github::GithubRepoName;
use crate::permissions::TeamApiClient;

pub mod client;
pub(crate) mod operations;

fn base_github_html_url() -> &'static str {
    "https://github.com"
}

pub fn create_github_client(
    app_id: AppId,
    github_url: String,
    private_key: SecretString,
) -> anyhow::Result<Octocrab> {
    let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.expose_secret().as_bytes())
        .context("Could not encode private key")?;

    Octocrab::builder()
        .base_uri(github_url)?
        .app(app_id, key)
        .build()
        .context("Could not create octocrab builder")
}

pub fn create_github_client_from_access_token(
    github_url: String,
    access_token: SecretString,
) -> anyhow::Result<Octocrab> {
    Octocrab::builder()
        .base_uri(github_url)?
        .user_access_token(access_token)
        .build()
        .context("Could not create octocrab builder")
}

/// Loads repositories that are connected to the given GitHub App client.
/// The anyhow::Result<RepositoryState> is intended, because we wanted to have
/// a hard error when the repos fail to load when the bot starts, but only log
/// a warning when we reload the state during the bot's execution.
pub async fn load_repositories(
    gh_client: &Octocrab,
    ci_client: Option<Octocrab>,
    team_api_client: &TeamApiClient,
) -> anyhow::Result<HashMap<GithubRepoName, anyhow::Result<RepositoryState>>> {
    let installations = gh_client
        .apps()
        .installations()
        .send()
        .await
        .context("Could not load app installations")?;

    // installation client can not be used to load current app
    // https://docs.github.com/en/rest/apps/apps?apiVersion=2022-11-28#get-the-authenticated-app
    let app = gh_client
        .current()
        .app()
        .await
        .context("Could not load Github App")?;

    let mut repositories = HashMap::default();
    for installation in installations {
        let installation_client = gh_client
            .installation(installation.id)
            .context("failed to install client")?;

        let repos = match load_installation_repos(&installation_client).await {
            Ok(repos) => repos,
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "Could not load repositories of installation {}: {error:?}",
                    installation.id
                ));
            }
        };
        for repo in repos {
            let repo_name = match parse_repo_name(&repo) {
                Ok(name) => name,
                Err(error) => {
                    tracing::error!("Found repository without a name: {error:?}");
                    continue;
                }
            };
            if repositories.contains_key(&repo_name) {
                return Err(anyhow::anyhow!(
                    "Repository {repo_name} found in multiple installations!",
                ));
            }
            let repo_state = create_repo_state(
                app.clone(),
                installation_client.clone(),
                ci_client.clone(),
                team_api_client,
                repo_name.clone(),
                &repo,
            )
            .await
            .map_err(|error| {
                anyhow::anyhow!("Cannot load repository {:?}: {error:?}", repo.full_name)
            });
            repositories.insert(repo_name, repo_state);
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

fn parse_repo_name(repo: &Repository) -> anyhow::Result<GithubRepoName> {
    let Some(owner) = repo.owner.clone() else {
        return Err(anyhow::anyhow!("Repository {} has no owner", repo.name));
    };

    Ok(GithubRepoName::new(&owner.login, &repo.name))
}

async fn create_repo_state(
    app: App,
    gh_app_installation_client: Octocrab,
    ci_client: Option<Octocrab>,
    team_api_client: &TeamApiClient,
    name: GithubRepoName,
    repo: &Repository,
) -> anyhow::Result<RepositoryState> {
    tracing::info!("Found repository {name}");

    let ci_client = ci_client.unwrap_or(gh_app_installation_client.clone());

    let main_repo_client = GithubRepositoryClient::new(
        app.clone(),
        gh_app_installation_client.clone(),
        name.clone(),
        repo.clone(),
    );
    let config = load_config(&main_repo_client).await?;
    let ci_repo = get_ci_repo(config.ci_repo.clone(), repo.clone(), &ci_client).await?;

    let ci_repo_client = GithubRepositoryClient::new(
        app,
        ci_client,
        config.ci_repo.clone().unwrap_or(name.clone()),
        ci_repo,
    );

    let permissions = team_api_client
        .load_permissions(&name)
        .await
        .with_context(|| format!("Could not load permissions for repository {name}"))?;

    Ok(RepositoryState {
        client: main_repo_client,
        ci_client: ci_repo_client,
        config: ArcSwap::new(Arc::new(config)),
        permissions: ArcSwap::new(Arc::new(permissions)),
    })
}

async fn load_config(client: &GithubRepositoryClient) -> anyhow::Result<RepositoryConfig> {
    let name = client.repository();
    match client.load_config().await {
        Ok(config) => {
            tracing::info!("Loaded repository config for {name}: {config:#?}");
            Ok(config)
        }
        Err(error) => Err(anyhow::anyhow!(
            "Could not load repository config for {name}: {error:?}"
        )),
    }
}

async fn get_ci_repo(
    ci_repo_name: Option<GithubRepoName>,
    default_client: Repository,
    ci_client: &Octocrab,
) -> anyhow::Result<Repository> {
    let Some(ci_repo_name) = ci_repo_name else {
        return Ok(default_client);
    };
    let ci_repo = ci_client
        .repos(ci_repo_name.owner(), ci_repo_name.name())
        .get()
        .await?;
    Ok(ci_repo)
}
