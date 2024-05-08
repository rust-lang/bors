use base64::Engine;
use serde::Serialize;
use url::Url;
use wiremock::{
    matchers::{method, path, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use super::user::{default_user, Author};

#[derive(Serialize)]
pub(super) struct Repository {
    id: u64,
    pub(crate) name: String,
    url: Url,
    pub(crate) owner: Author,
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

#[derive(Serialize)]
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

pub(super) async fn setup_repositories_mock(mock_server: &MockServer) {
    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Repositories::default()))
        .mount(mock_server)
        .await;
}

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

pub(super) async fn setup_repository_config_mock(mock_server: &MockServer) {
    Mock::given(method("GET"))
        // regex to match all repositories at /repos/:owner/:repo/contents/rust-bors.toml
        .and(path_regex(r"^/repos/[^/]+/[^/]+/contents/rust-bors.toml$"))
        .respond_with(ResponseTemplate::new(200).set_body_json(Content::default()))
        .mount(mock_server)
        .await;
}
