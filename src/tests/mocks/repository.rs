use base64::Engine;
use serde::Serialize;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

use crate::tests::mocks::World;

use super::user::GitHubUser;

/// Handles all repositories related requests
#[derive(Default)]
pub struct RepositoriesHandler;

impl RepositoriesHandler {
    pub async fn mount(&self, world: &World, mock_server: &MockServer) {
        let repos = Repositories {
            total_count: world.repos.len() as u64,
            repositories: world
                .repos
                .iter()
                .enumerate()
                .map(|(index, (_, repo))| Repository {
                    id: index as u64,
                    owner: GitHubUser::new(repo.name.owner()),
                    name: repo.name.name().to_string(),
                    url: format!("https://{}.foo", repo.name.name()).parse().unwrap(),
                })
                .collect(),
        };

        Mock::given(method("GET"))
            .and(path("/installation/repositories"))
            .respond_with(ResponseTemplate::new(200).set_body_json(repos))
            .mount(mock_server)
            .await;

        for repo in world.repos.values() {
            Mock::given(method("GET"))
                .and(path(format!(
                    "/repos/{}/contents/rust-bors.toml",
                    repo.name
                )))
                .respond_with(
                    ResponseTemplate::new(200)
                        .set_body_json(Content::new("rust-bors.toml", &repo.config)),
                )
                .mount(mock_server)
                .await;
        }
    }
}

/// Represents all repositories for an installation
/// Returns type for the `GET /installation/repositories` endpoint
#[derive(Serialize)]
struct Repositories {
    total_count: u64,
    repositories: Vec<Repository>,
}

#[derive(Serialize)]
pub(super) struct Repository {
    id: u64,
    name: String,
    url: Url,
    owner: GitHubUser,
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

impl Content {
    fn new(path: &str, content: &str) -> Self {
        let content = base64::prelude::BASE64_STANDARD.encode(content);
        let size = content.len() as i64;
        Content {
            name: path.to_string(),
            path: path.to_string(),
            sha: "test".to_string(),
            encoding: Some("base64".to_string()),
            content: Some(content),
            size,
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
