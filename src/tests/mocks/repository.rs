use std::fmt;

use base64::Engine;
use serde::Serialize;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::github::GithubRepoName;

use super::user::{default_user, User};

/// Handles all repositories related requests
#[derive(Default)]
pub(super) struct RepositoriesHandler {}

impl RepositoriesHandler {
    pub(super) async fn mount(&self, mock_server: &MockServer) {
        let repos = Repositories::default();
        Mock::given(method("GET"))
            .and(path("/installation/repositories"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repos.clone()))
            .mount(mock_server)
            .await;

        for repo in repos.repositories {
            Mock::given(method("GET"))
                .and(path(format!("/repos/{}/contents/rust-bors.toml", repo)))
                .respond_with(ResponseTemplate::new(200).set_body_json(Content::default()))
                .mount(mock_server)
                .await;
        }
    }
}

/// Represents all repositories for an installation
/// Returns type for the `GET /installation/repositories` endpoint
#[derive(Clone, Serialize)]
struct Repositories {
    total_count: u64,
    repositories: Vec<Repository>,
}

impl Default for Repositories {
    fn default() -> Self {
        Repositories {
            total_count: 1,
            repositories: vec![Repository::default()],
        }
    }
}

#[derive(Clone, Serialize)]
pub(super) struct Repository {
    id: u64,
    pub(crate) name: String,
    url: Url,
    pub(crate) owner: User,
}

impl Default for Repository {
    fn default() -> Self {
        Repository {
            id: 1,
            name: "test".to_string(),
            url: "https://test.com".parse().unwrap(),
            owner: default_user(),
        }
    }
}

impl fmt::Display for Repository {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = GithubRepoName::new(&self.owner.login, &self.name);
        write!(f, "{}", name)
    }
}

/// Represents a file in a GitHub repository
/// returns type for the `GET /repos/{owner}/{repo}/contents/{path}` endpoint
#[derive(Serialize)]
struct Content {
    name: String,
    path: String,
    sha: String,
    encoding: Option<String>,
    content: Option<String>,
    size: i64,
    url: String,
    r#type: String,
    #[serde(rename = "_links")]
    links: ContentLinks,
}

#[derive(Serialize)]
struct ContentLinks {
    #[serde(rename = "self")]
    _self: Url,
}

impl Default for ContentLinks {
    fn default() -> Self {
        ContentLinks {
            _self: "https://test.com".parse().unwrap(),
        }
    }
}

const RUST_BORS_TOML: &str = r###"
timeout = 3600

[labels]
try = ["+foo", "-bar"]
try_succeed = ["+foobar", "+foo", "+baz"]
try_failed = []
"###;

impl Default for Content {
    fn default() -> Self {
        let content = base64::prelude::BASE64_STANDARD.encode(RUST_BORS_TOML);
        Content {
            name: "test".to_string(),
            path: "test".to_string(),
            sha: "test".to_string(),
            encoding: Some("base64".to_string()),
            content: Some(content),
            size: 1,
            url: "https://test.com".to_string(),
            r#type: "file".to_string(),
            links: ContentLinks {
                _self: "https://test.com".parse().unwrap(),
            },
        }
    }
}
