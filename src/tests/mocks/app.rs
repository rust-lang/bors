use serde::Serialize;
use url::Url;
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use super::user::{default_user, Author};

#[derive(Serialize)]
struct App {
    pub(crate) id: u64,
    node_id: String,
    owner: Author,
    name: String,
    external_url: Url,
    html_url: Url,
    permissions: Permissions,
    events: Vec<String>,
}

impl Default for App {
    fn default() -> Self {
        App {
            id: 1,
            node_id: "".to_string(),
            owner: default_user(),
            name: "test".to_string(),
            // same as bors user html_url
            html_url: "https://test-bors.bot.com".parse().unwrap(),
            external_url: "https://test-bors.bot.com".parse().unwrap(),
            permissions: Permissions {},
            events: vec!["*".to_string()],
        }
    }
}

pub(super) async fn setup_app_mock(mock_server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/app"))
        .respond_with(ResponseTemplate::new(200).set_body_json(App::default()))
        .mount(mock_server)
        .await;
}

#[derive(Serialize)]
pub(crate) struct Installation {
    pub(crate) id: u64,
    node_id: String,
    account: Author,
    permissions: Permissions,
    events: Vec<String>,
}

impl Default for Installation {
    fn default() -> Self {
        Installation {
            id: 1,
            node_id: "".to_string(),
            account: default_user(),
            permissions: Permissions {},
            events: vec!["*".to_string()],
        }
    }
}

pub(super) async fn setup_app_installation_mock(mock_server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/app/installations"))
        .respond_with(ResponseTemplate::new(200).set_body_json(vec![Installation::default()]))
        .mount(mock_server)
        .await;
}

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

pub(super) async fn setup_app_installation_token_mock(mock_server: &MockServer) {
    Mock::given(method("POST"))
        .and(path_regex("^/app/installations/\\d+/access_tokens$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(InstallationToken::default()))
        .mount(mock_server)
        .await;
}

#[derive(Serialize)]
pub(crate) struct Permissions {}
