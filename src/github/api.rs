use std::collections::HashMap;

use anyhow::Context;
use octocrab::models::issues::Comment;
use octocrab::models::{AppId, InstallationRepositories};
use octocrab::Octocrab;
use secrecy::{ExposeSecret, SecretVec};

use crate::github::GithubRepoName;

/// Provides access to managed GitHub repositories.
#[derive(Debug)]
pub struct RepositoryAccess {
    repositories: HashMap<GithubRepoName, Repository>,
}

impl RepositoryAccess {
    /// Loads repositories managed by the Bors GitHub app with the given ID.
    pub async fn load_repositories(
        app_id: AppId,
        private_key: SecretVec<u8>,
    ) -> anyhow::Result<RepositoryAccess> {
        let key = jsonwebtoken::EncodingKey::from_rsa_pem(private_key.expose_secret().as_ref())
            .context("Cannot encode private key")?;

        let octocrab = Octocrab::builder()
            .app(app_id, key)
            .build()
            .context("Cannot create octocrab builder")?;

        let installations = octocrab
            .apps()
            .installations()
            .send()
            .await
            .context("Cannot load app installations")?;

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
                    let access = Repository {
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
        Ok(RepositoryAccess { repositories })
    }

    pub fn get_repo(&self, key: &GithubRepoName) -> Option<&Repository> {
        self.repositories.get(key)
    }
}

// HTML marker that states that a comment was made by this bot.
const BORS_MARKER: &str = "<!-- bors -->";

/// Provides access to a single app installation (repository).
#[derive(Debug)]
pub struct Repository {
    /// The client caches the access token for this given repository and refreshes it once it
    /// expires.
    client: Octocrab,
    repository: GithubRepoName,
}

impl Repository {
    /// Returns true if the comment was made by this bot.
    pub fn is_comment_internal(&self, comment: &Comment) -> bool {
        let comment_html = comment.body.as_deref().unwrap_or_default();
        comment.user.r#type.to_ascii_lowercase() == "bot" && comment_html.contains(BORS_MARKER)
    }

    pub async fn post_pr_comment(&self, issue: u64, content: &str) -> octocrab::Result<Comment> {
        let content = format!("{content}\n{BORS_MARKER}");

        self.client
            .issues(&self.repository.owner, &self.repository.name)
            .create_comment(issue, content)
            .await
    }

    pub fn repository(&self) -> &GithubRepoName {
        &self.repository
    }
}
