use std::collections::HashMap;
use std::sync::Arc;

use base64::Engine;
use octocrab::models::repos::Object;
use octocrab::models::repos::Object::Commit;
use parking_lot::Mutex;
use serde::Serialize;
use tokio::sync::mpsc::Sender;
use url::Url;
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, Request, ResponseTemplate,
};

use crate::github::GithubRepoName;
use crate::permissions::PermissionType;
use crate::tests::event::default_pr_number;
use crate::tests::mocks::comment::Comment;
use crate::tests::mocks::permissions::Permissions;
use crate::tests::mocks::pull_request::mock_pull_requests;
use crate::tests::mocks::{dynamic_mock_req, World};

use super::user::{GitHubUser, User};

#[derive(Clone)]
pub struct Repo {
    pub name: GithubRepoName,
    pub permissions: Permissions,
    pub config: String,
    // Pre-set known PRs to avoid responding to requests about PRs that we
    // don't expect.
    pub known_prs: Vec<u64>,
    pub branches: Vec<Branch>,
}

impl Repo {
    pub fn new(owner: &str, name: &str, permissions: Permissions, config: String) -> Self {
        Self {
            name: GithubRepoName::new(owner, name),
            permissions,
            config,
            known_prs: vec![default_pr_number()],
            branches: vec![Branch::default()],
        }
    }

    pub fn with_perms(mut self, user: User, permissions: &[PermissionType]) -> Self {
        self.permissions.users.insert(user, permissions.to_vec());
        self
    }

    pub fn get_branch(&mut self, name: &str) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| b.name == name)
    }
}

impl Default for Repo {
    fn default() -> Self {
        let config = r#"
timeout = 3600
"#
        .to_string();
        let mut users = HashMap::default();
        users.insert(
            User::default(),
            vec![PermissionType::Try, PermissionType::Review],
        );

        Self::new(
            default_repo_name().owner(),
            default_repo_name().name(),
            Permissions { users },
            config,
        )
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("rust-lang", "borstest")
}

#[derive(Clone)]
pub struct Branch {
    name: String,
    sha: String,
    commit_message: String,
    sha_history: Vec<String>,
}

impl Branch {
    pub fn new(name: &str, sha: &str) -> Self {
        Self {
            name: name.to_string(),
            sha: sha.to_string(),
            commit_message: format!("Commit {sha}"),
            sha_history: vec![],
        }
    }

    pub fn set_to_sha(&mut self, sha: &str) {
        self.sha_history.push(self.sha.clone());
        self.sha = sha.to_string();
    }

    pub fn get_sha_history(&self) -> Vec<String> {
        let mut shas = self.sha_history.clone();
        shas.push(self.sha.clone());
        shas
    }
}

impl Default for Branch {
    fn default() -> Self {
        Self::new(&default_branch_name(), &default_branch_sha())
    }
}

pub fn default_branch_name() -> &'static str {
    "main"
}

pub fn default_branch_sha() -> &'static str {
    "main-sha1"
}

pub async fn mock_repo_list(world: &World, mock_server: &MockServer) {
    let repos = GitHubRepositories {
        total_count: world.repos.len() as u64,
        repositories: world
            .repos
            .iter()
            .enumerate()
            .map(|(index, (_, repo))| {
                let repo = repo.lock();
                GitHubRepository {
                    id: index as u64,
                    owner: User::new(index as u64, repo.name.owner()).into(),
                    name: repo.name.name().to_string(),
                    url: format!("https://{}.foo", repo.name.name()).parse().unwrap(),
                }
            })
            .collect(),
    };

    Mock::given(method("GET"))
        .and(path("/installation/repositories"))
        .respond_with(ResponseTemplate::new(200).set_body_json(repos))
        .mount(mock_server)
        .await;
}

pub async fn mock_repo(
    repo: Arc<Mutex<Repo>>,
    comments_tx: Sender<Comment>,
    mock_server: &MockServer,
) {
    mock_pull_requests(repo.clone(), comments_tx, mock_server).await;
    mock_branches(repo.clone(), mock_server).await;
    mock_config(repo, mock_server).await;
}

async fn mock_branches(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    mock_get_branch(repo.clone(), mock_server).await;
    mock_set_branch_to_ref(repo.clone(), mock_server).await;
    mock_merge_branch(repo, mock_server).await;
}

async fn mock_get_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    dynamic_mock_req(
        move |_req: &Request, [branch_name]: [&str; 1]| {
            let mut repo = repo.lock();
            let Some(branch) = repo.get_branch(branch_name) else {
                return ResponseTemplate::new(404);
            };
            let branch = GitHubBranch {
                name: branch.name.clone(),
                commit: GitHubCommitObject {
                    sha: branch.sha.clone(),
                    url: format!("https://github.com/branch/{}-{}", branch.name, branch.sha)
                        .parse()
                        .unwrap(),
                },
                protected: false,
            };
            ResponseTemplate::new(200).set_body_json(branch)
        },
        "GET",
        format!("^/repos/{repo_name}/branches/(.*)$"),
    )
    .mount(mock_server)
    .await;
}

async fn mock_set_branch_to_ref(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo_name = repo.lock().name.clone();
    Mock::given(method("POST"))
        .and(path(format!("/repos/{repo_name}/git/refs")))
        .respond_with(move |request: &Request| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct SetRefRequest {
                r#ref: String,
                sha: String,
            }

            let data: SetRefRequest = request.body_json().unwrap();
            let branch_name = data
                .r#ref
                .strip_prefix("refs/heads/")
                .expect("Unexpected ref name");

            let sha = data.sha;
            match repo.get_branch(branch_name) {
                Some(branch) => {
                    // Change sha of branch
                    branch.set_to_sha(&sha);
                }
                None => {
                    // Create a new branch
                    repo.branches.push(Branch::new(&branch_name, &sha));
                }
            }

            let url: Url = format!("https://github.com/branches/{branch_name}")
                .parse()
                .unwrap();
            let response = GitHubRef {
                ref_field: data.r#ref,
                node_id: repo.branches.len().to_string(),
                url: url.clone(),
                object: Commit { sha, url },
            };
            ResponseTemplate::new(200).set_body_json(response)
        })
        .mount(mock_server)
        .await;
}

async fn mock_merge_branch(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(format!("/repos/{}/merges", repo.lock().name)))
        .respond_with(move |request: &Request| {
            let mut repo = repo.lock();

            #[derive(serde::Deserialize)]
            struct MergeRequest {
                base: String,
                head: String,
                commit_message: String,
            }

            let data: MergeRequest = request.body_json().unwrap();
            let head = repo.get_branch(&data.head);
            let head_sha = match head {
                None => {
                    // head is a SHA
                    data.head
                }
                Some(branch) => {
                    // head is a branch
                    branch.sha.clone()
                }
            };
            let Some(base_branch) = repo.get_branch(&data.base) else {
                return ResponseTemplate::new(404);
            };
            let merge_sha = format!("merge-{}-{head_sha}", base_branch.sha);
            base_branch.set_to_sha(&merge_sha);
            base_branch.commit_message = data.commit_message;

            #[derive(serde::Serialize)]
            struct MergeResponse {
                sha: String,
            }
            let response = MergeResponse { sha: merge_sha };
            ResponseTemplate::new(201).set_body_json(response)
        })
        .mount(mock_server)
        .await;
}

async fn mock_config(repo: Arc<Mutex<Repo>>, mock_server: &MockServer) {
    let repo = repo.lock();
    Mock::given(method("GET"))
        .and(path(format!(
            "/repos/{}/contents/rust-bors.toml",
            repo.name
        )))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(GitHubContent::new("rust-bors.toml", &repo.config)),
        )
        .mount(mock_server)
        .await;
}

/// Represents all repositories for an installation
/// Returns type for the `GET /installation/repositories` endpoint
#[derive(Serialize)]
struct GitHubRepositories {
    total_count: u64,
    repositories: Vec<GitHubRepository>,
}

#[derive(Serialize)]
pub struct GitHubRepository {
    id: u64,
    name: String,
    url: Url,
    owner: GitHubUser,
}

impl From<GithubRepoName> for GitHubRepository {
    fn from(value: GithubRepoName) -> Self {
        Self {
            id: 1,
            name: value.name().to_string(),
            owner: GitHubUser::new(value.owner(), 1001),
            url: format!("https://github.com/{}", value).parse().unwrap(),
        }
    }
}

/// Represents a file in a GitHub repository
/// returns type for the `GET /repos/{owner}/{repo}/contents/{path}` endpoint
#[derive(Serialize)]
struct GitHubContent {
    name: String,
    path: String,
    sha: String,
    encoding: Option<String>,
    content: Option<String>,
    size: i64,
    url: String,
    r#type: String,
    #[serde(rename = "_links")]
    links: GitHubContentLinks,
}

impl GitHubContent {
    fn new(path: &str, content: &str) -> Self {
        let content = base64::prelude::BASE64_STANDARD.encode(content);
        let size = content.len() as i64;
        GitHubContent {
            name: path.to_string(),
            path: path.to_string(),
            sha: "test".to_string(),
            encoding: Some("base64".to_string()),
            content: Some(content),
            size,
            url: "https://test.com".to_string(),
            r#type: "file".to_string(),
            links: GitHubContentLinks {
                _self: "https://test.com".parse().unwrap(),
            },
        }
    }
}

#[derive(Serialize)]
struct GitHubContentLinks {
    #[serde(rename = "self")]
    _self: Url,
}

#[derive(Serialize)]
struct GitHubBranch {
    name: String,
    commit: GitHubCommitObject,
    protected: bool,
}

#[derive(Serialize)]
struct GitHubCommitObject {
    sha: String,
    url: Url,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
struct GitHubRef {
    #[serde(rename = "ref")]
    ref_field: String,
    node_id: String,
    url: Url,
    object: Object,
}
