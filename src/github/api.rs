use std::collections::HashMap;

use crate::github::event::PullRequestComment;
use anyhow::Context;
use octocrab::models::issues::Comment;
use octocrab::models::{App, AppId, InstallationRepositories};
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
        comment.user.html_url == self.app.html_url
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
                        let Some(owner) = repo.owner else { continue; };
                        let repository = GithubRepoName::new(&owner.login, &repo.name);
                        let access = RepositoryClient {
                            client: installation_client.clone(),
                            repository: repository.clone(),
                        };

                        log::info!("Found repository {repository}");

                        if let Some(existing) = repositories.insert(repository, access) {
                            return Err(anyhow::anyhow!(
                                "Repository {} found in multiple installations!",
                                existing.repository
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

/// Provides access to a single app installation (repository).
#[derive(Debug)]
pub struct RepositoryClient {
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    client: Octocrab,
    repository: GithubRepoName,
}

impl RepositoryClient {
    pub async fn post_pr_comment(&self, issue: u64, content: &str) -> octocrab::Result<Comment> {
        self.client
            .issues(&self.repository.owner, &self.repository.name)
            .create_comment(issue, content)
            .await
    }

    pub fn repository(&self) -> &GithubRepoName {
        &self.repository
    }
}
