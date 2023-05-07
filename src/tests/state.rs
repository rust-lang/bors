use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::string::ToString;

use crate::config::RepositoryConfig;
use axum::async_trait;
use derive_builder::Builder;

use super::permissions::AllPermissions;
use crate::bors::event::{BorsEvent, CheckSuiteCompleted, PullRequestComment, WorkflowStarted};
use crate::bors::{handle_bors_event, CheckSuite, RepositoryState};
use crate::bors::{BorsState, RepositoryClient};
use crate::database::{DbClient, SeaORMClient};
use crate::github::{CommitSha, GithubRepoName, GithubUser, PullRequest};
use crate::github::{MergeError, PullRequestNumber};
use crate::permissions::PermissionResolver;
use crate::tests::database::create_test_db;
use crate::tests::github::PRBuilder;

pub fn test_bot_user() -> GithubUser {
    GithubUser {
        username: "<test-bot>".to_string(),
        html_url: "https://test-bors.bot.com".parse().unwrap(),
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("owner", "name")
}

pub struct TestBorsState {
    repos: HashMap<GithubRepoName, RepositoryState<TestRepositoryClient>>,
    pub db: SeaORMClient,
}

impl TestBorsState {
    /// Returns the default test client
    pub fn client(&mut self) -> &mut TestRepositoryClient {
        &mut self.repos.get_mut(&default_repo_name()).unwrap().client
    }

    /// Execute an event.
    pub async fn event(&mut self, event: BorsEvent) {
        handle_bors_event(event, self).await.unwrap();
    }

    pub async fn comment<T: Into<PullRequestComment>>(&mut self, comment: T) {
        self.event(BorsEvent::Comment(comment.into())).await;
    }

    pub async fn workflow_started<T: Into<WorkflowStarted>>(&mut self, payload: T) {
        self.event(BorsEvent::WorkflowStarted(payload.into())).await;
    }

    pub async fn check_suite_completed<T: Into<CheckSuiteCompleted>>(&mut self, payload: T) {
        self.event(BorsEvent::CheckSuiteCompleted(payload.into()))
            .await;
    }
}

impl BorsState<TestRepositoryClient> for TestBorsState {
    fn is_comment_internal(&self, comment: &PullRequestComment) -> bool {
        comment.author == test_bot_user()
    }

    fn get_repo_state_mut(
        &mut self,
        repo: &GithubRepoName,
    ) -> Option<(
        &mut RepositoryState<TestRepositoryClient>,
        &mut dyn DbClient,
    )> {
        self.repos
            .get_mut(repo)
            .map(|repo| (repo, (&mut self.db) as &mut dyn DbClient))
    }

    fn reload_repositories(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + '_>> {
        Box::pin(async move { Ok(()) })
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Client {
    #[builder(default)]
    name: Option<GithubRepoName>,
    #[builder(default = "Box::new(AllPermissions)")]
    permission_resolver: Box<dyn PermissionResolver>,
    #[builder(default)]
    config: Option<RepositoryConfig>,
}

impl ClientBuilder {
    pub async fn create_state(self) -> TestBorsState {
        self.create().into_state().await
    }
    pub fn create(self) -> RepositoryState<TestRepositoryClient> {
        let Client {
            name,
            permission_resolver,
            config,
        } = self.build().unwrap();

        let name = name.unwrap_or_else(default_repo_name);
        RepositoryState {
            repository: name.clone(),
            client: TestRepositoryClient {
                comments: Default::default(),
                name,
                merge_branches_fn: Box::new(|| Ok(CommitSha("foo".to_string()))),
                get_pr_fn: Box::new(move |pr| Ok(PRBuilder::default().number(pr.0).create())),
                check_suites: Default::default(),
            },
            permissions_resolver: permission_resolver,
            config: config.unwrap_or_else(|| RepositoryConfig { checks: vec![] }),
        }
    }
}

impl RepositoryState<TestRepositoryClient> {
    pub async fn into_state(self) -> TestBorsState {
        let mut repos = HashMap::new();
        repos.insert(self.repository.clone(), self);
        TestBorsState {
            repos,
            db: create_test_db().await,
        }
    }
}

pub struct TestRepositoryClient {
    pub name: GithubRepoName,
    comments: HashMap<u64, Vec<String>>,
    pub merge_branches_fn: Box<dyn Fn() -> Result<CommitSha, MergeError> + Send>,
    pub get_pr_fn: Box<dyn Fn(PullRequestNumber) -> anyhow::Result<PullRequest> + Send>,
    pub check_suites: HashMap<String, Vec<CheckSuite>>,
}

impl TestRepositoryClient {
    pub fn get_comment(&self, pr_number: u64, comment_index: usize) -> &str {
        &self.comments.get(&pr_number).unwrap()[comment_index]
    }
    pub fn get_last_comment(&self, pr_number: u64) -> &str {
        self.comments
            .get(&pr_number)
            .unwrap()
            .last()
            .unwrap()
            .as_str()
    }
    pub fn check_comments(&self, pr_number: u64, comments: &[&str]) {
        assert_eq!(
            self.comments.get(&pr_number).cloned().unwrap_or_default(),
            comments
                .iter()
                .map(|&s| String::from(s))
                .collect::<Vec<_>>()
        );
    }
    pub fn check_comment_count(&self, pr_number: u64, count: usize) {
        assert_eq!(
            self.comments
                .get(&pr_number)
                .cloned()
                .unwrap_or_default()
                .len(),
            count
        );
    }
}

#[async_trait]
impl RepositoryClient for TestRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn get_pull_request(&mut self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        (self.get_pr_fn)(pr)
    }

    async fn post_comment(&mut self, pr: PullRequestNumber, text: &str) -> anyhow::Result<()> {
        self.comments
            .entry(pr.0)
            .or_default()
            .push(text.to_string());
        Ok(())
    }

    async fn set_branch_to_sha(&mut self, _branch: &str, _sha: &CommitSha) -> anyhow::Result<()> {
        Ok(())
    }

    async fn merge_branches(
        &mut self,
        _base: &str,
        _head: &CommitSha,
        _commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        (self.merge_branches_fn)()
    }

    async fn get_check_suites_for_commit(
        &mut self,
        _branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>> {
        Ok(self.check_suites.get(&sha.0).cloned().unwrap_or_default())
    }
}
