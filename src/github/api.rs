use std::collections::HashMap;

use anyhow::Context;
use octocrab::models::issues::Comment;
use octocrab::models::{App, AppId, InstallationRepositories};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretVec};

use crate::github::GithubRepoName;

/// Provides access to managed GitHub repositories.
#[derive(Debug)]
pub struct GithubAppClient {
    app: App,
    repositories: HashMap<GithubRepoName, RepositoryClient>,
}

impl GithubAppClient {
    /// Loads repositories managed by the Bors GitHub app with the given ID.
    pub async fn load_repositories(
        app_id: AppId,
        private_key: SecretVec<u8>,
    ) -> anyhow::Result<GithubAppClient> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.expose_secret().as_ref())
            .context("Could not encode private key")?;

        let octocrab = Octocrab::builder()
            .app(app_id, key)
            .build()
            .context("Could not create octocrab builder")?;

        let app = octocrab
            .current()
            .app()
            .await
            .context("Could not load Github App")?;

        let installations = octocrab
            .apps()
            .installations()
            .send()
            .await
            .context("Could not load app installations")?;

        let mut repositories = HashMap::default();
        for installation in installations {
            if let Some(ref repositories_url) = installation.repositories_url {
                let installation_client = octocrab.installation(installation.id);

                // Load repositories of this installation
                let repos: InstallationRepositories = installation_client
                    .get::<_, _, ()>(repositories_url, None)
                    .await?;
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
        }
        Ok(GithubAppClient { app, repositories })
    }

    pub fn get_repo(&self, key: &GithubRepoName) -> Option<&RepositoryClient> {
        self.repositories.get(key)
    }

    /// Returns true if the comment was made by this bot.
    pub fn is_comment_internal(&self, comment: &Comment) -> bool {
        comment.user.html_url == self.app.html_url
    }
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
