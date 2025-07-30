use crate::tests::User;
use serde::Serialize;
use url::Url;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path, path_regex},
};

use super::user::GitHubUser;

/// Handles all app related requests
#[derive(Default)]
pub(crate) struct AppHandler {}

impl AppHandler {
    pub(super) async fn mount(&self, mock_server: &MockServer) {
        Mock::given(method("GET"))
            .and(path("/app"))
            .respond_with(ResponseTemplate::new(200).set_body_json(GitHubApp::default()))
            .mount(mock_server)
            .await;
        Mock::given(method("GET"))
            .and(path("/app/installations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(vec![Installation::default()]))
            .mount(mock_server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("^/app/installations/\\d+/access_tokens$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(InstallationToken::default()))
            .mount(mock_server)
            .await;
    }
}

/// Represents an app on GitHub
/// Returns type for the `GET /app` endpoint
#[derive(Serialize)]
pub struct GitHubApp {
    id: u64,
    node_id: String,
    pub owner: GitHubUser,
    name: String,
    external_url: Url,
    html_url: Url,
    permissions: Permissions,
    events: Vec<String>,
}

impl Default for GitHubApp {
    fn default() -> Self {
        GitHubApp {
            id: default_app_id(),
            node_id: "1234".to_string(),
            owner: GitHubUser::default(),
            name: "bors".to_string(),
            // same as bors user html_url
            html_url: GitHubUser::from(User::bors_bot()).html_url,
            external_url: "https://test-bors.bot.com".parse().unwrap(),
            permissions: Permissions {},
            events: vec!["*".to_string()],
        }
    }
}

pub fn default_app_id() -> u64 {
    1
}

/// Represents an installation of an app on GitHub
/// Returns type for the `GET /app/installations` endpoint
#[derive(Serialize)]
pub(crate) struct Installation {
    id: u64,
    node_id: String,
    account: GitHubUser,
    permissions: Permissions,
    events: Vec<String>,
}

impl Default for Installation {
    fn default() -> Self {
        Installation {
            id: 1,
            node_id: "".to_string(),
            account: GitHubUser::default(),
            permissions: Permissions {},
            events: vec!["*".to_string()],
        }
    }
}

/// Represents an installation token for an app on GitHub
/// Returns type for the `POST /app/installations/{installation_id}/access_tokens` endpoint
#[derive(Serialize)]
pub(crate) struct InstallationToken {
    token: String,
    permissions: Permissions,
}

impl Default for InstallationToken {
    fn default() -> Self {
        InstallationToken {
            token: "test".to_string(),
            permissions: Permissions {},
        }
    }
}

#[derive(Serialize)]
pub(crate) struct Permissions {}
