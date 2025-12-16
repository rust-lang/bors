use anyhow::Context;
use octocrab::{Octocrab, OctocrabBuilder};
use secrecy::{ExposeSecret, SecretString};
use std::collections::HashMap;

/// Client that handles OAuth authentication used for rollups.
/// It is able to provide a GitHub client authenticated as a given GitHub user.
#[derive(Clone)]
pub struct OAuthClient {
    config: OAuthConfig,
    github_base_url: String,
}

impl OAuthClient {
    pub fn new(config: OAuthConfig, github_base_url: String) -> Self {
        Self {
            config,
            github_base_url,
        }
    }

    pub fn config(&self) -> &OAuthConfig {
        &self.config
    }

    /// Create a GitHub client authenticated as a user with the given OAuth exchange code.
    pub async fn get_authenticated_client(&self, code: &str) -> anyhow::Result<Octocrab> {
        tracing::info!("Exchanging OAuth code for access token");
        let client = reqwest::Client::new();
        let token_response = client
            .post(format!("{}/login/oauth/access_token", self.github_base_url))
            .form(&[
                ("client_id", self.config.client_id()),
                ("client_secret", self.config.client_secret()),
                ("code", code),
            ])
            .send()
            .await
            .context("Failed to send OAuth token exchange request to GitHub")?
            .text()
            .await
            .context("Failed to read OAuth token response from GitHub")?;

        let oauth_token_params: HashMap<String, String> =
            url::form_urlencoded::parse(token_response.as_bytes())
                .into_owned()
                .collect();
        let access_token = oauth_token_params
            .get("access_token")
            .ok_or_else(|| anyhow::anyhow!("No OAuth access token in response"))?;

        tracing::info!("Retrieved OAuth access token");

        let user_client = OctocrabBuilder::new()
            .user_access_token(access_token.clone())
            .build()?;

        Ok(user_client)
    }
}

#[derive(Clone)]
pub struct OAuthConfig {
    client_id: String,
    client_secret: SecretString,
}

impl OAuthConfig {
    pub fn new(client_id: String, client_secret: String) -> Self {
        Self {
            client_id,
            client_secret: client_secret.into(),
        }
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    fn client_secret(&self) -> &str {
        self.client_secret.expose_secret()
    }
}
