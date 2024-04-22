use std::collections::{HashMap, HashSet};
use std::string::ToString;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use axum::async_trait;
use derive_builder::Builder;
use octocrab::models::{RunId, UserId};
use url::Url;

use super::database::MockedDBClient;
use super::event::default_user;
use crate::bors::event::{
    BorsEvent, CheckSuiteCompleted, PullRequestComment, WorkflowCompleted, WorkflowStarted,
};
use crate::bors::{
    handle_bors_event, BorsContext, CheckSuite, CommandParser, Comment, RepositoryState,
};
use crate::bors::{BorsState, RepositoryClient};
use crate::config::RepositoryConfig;
use crate::database::{DbClient, WorkflowStatus};
use crate::github::{
    CommitSha, GithubRepoName, GithubUser, LabelModification, LabelTrigger, PullRequest,
};
use crate::github::{MergeError, PullRequestNumber};
use crate::permissions::UserPermissions;
use crate::tests::database::create_test_db;
use crate::tests::event::{
    CheckSuiteCompletedBuilder, WorkflowCompletedBuilder, WorkflowStartedBuilder,
};
use crate::tests::github::{default_base_branch, PRBuilder};

pub fn test_bot_user() -> GithubUser {
    GithubUser {
        // just a random one, to reduce the chance of duplicate id
        id: UserId(517237103),
        username: "<test-bot>".to_string(),
        html_url: "https://test-bors.bot.com".parse().unwrap(),
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("owner", "name")
}

pub fn default_merge_sha() -> String {
    "sha-merged".to_string()
}

type TestRepositoryState = RepositoryState<Arc<TestRepositoryClient>>;

#[derive(Clone)]
pub struct TestBorsState {
    default_client: Arc<TestRepositoryClient>,
    repos: Arc<HashMap<GithubRepoName, Arc<TestRepositoryState>>>,
    pub db: Arc<MockedDBClient>,
}

impl TestBorsState {
    /// Returns the default test client
    pub fn client(&self) -> &TestRepositoryClient {
        &self.default_client
    }

    /// Execute an event.
    pub async fn event(&self, event: BorsEvent) {
        handle_bors_event(
            event,
            Arc::new(self.clone()),
            Arc::new(BorsContext::new(
                CommandParser::new("@bors".to_string()),
                Arc::clone(&self.db) as Arc<dyn DbClient>,
            )),
        )
        .await
        .unwrap();
    }

    pub async fn comment<T: Into<PullRequestComment>>(&self, comment: T) {
        self.event(BorsEvent::Comment(comment.into())).await;
    }

    pub async fn workflow_started<T: Into<WorkflowStarted>>(&self, payload: T) {
        self.event(BorsEvent::WorkflowStarted(payload.into())).await;
    }

    pub async fn workflow_completed<T: Into<WorkflowCompleted>>(&self, payload: T) {
        self.event(BorsEvent::WorkflowCompleted(payload.into()))
            .await;
    }

    pub async fn check_suite_completed<T: Into<CheckSuiteCompleted>>(&self, payload: T) {
        self.event(BorsEvent::CheckSuiteCompleted(payload.into()))
            .await;
    }

    pub async fn refresh(&mut self) {
        self.event(BorsEvent::Refresh).await;
    }

    pub async fn perform_workflow_events(
        &self,
        run_id: u64,
        branch: &str,
        commit: &str,
        status: WorkflowStatus,
    ) {
        let name = format!("workflow-{run_id}");
        self.workflow_started(
            WorkflowStartedBuilder::default()
                .branch(branch.to_string())
                .commit_sha(commit.to_string())
                .name(name.to_string())
                .url(Some(format!("https://{name}.com")))
                .run_id(run_id),
        )
        .await;
        self.workflow_completed(
            WorkflowCompletedBuilder::default()
                .branch(branch.to_string())
                .commit_sha(commit.to_string())
                .run_id(run_id)
                .status(status),
        )
        .await;
        self.check_suite_completed(
            CheckSuiteCompletedBuilder::default()
                .branch(branch.to_string())
                .commit_sha(commit.to_string()),
        )
        .await;
    }
}

#[async_trait]
impl BorsState<Arc<TestRepositoryClient>> for TestBorsState {
    fn get_repo_state(&self, repo: &GithubRepoName) -> Option<Arc<TestRepositoryState>> {
        self.repos.get(repo).map(|repo| Arc::clone(repo))
    }

    fn get_all_repos(&self) -> Vec<Arc<TestRepositoryState>> {
        self.repos.values().cloned().collect()
    }

    async fn reload_repositories(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct RepoConfig {
    #[builder(default = "Duration::from_secs(3600)")]
    timeout: Duration,
    #[builder(field(ty = "HashMap<LabelTrigger, Vec<LabelModification>>"))]
    labels: HashMap<LabelTrigger, Vec<LabelModification>>,
}

impl RepoConfigBuilder {
    pub fn add_label(mut self, trigger: LabelTrigger, label: &str) -> Self {
        self.labels
            .entry(trigger)
            .or_default()
            .push(LabelModification::Add(label.to_string()));
        self
    }

    pub fn remove_label(mut self, trigger: LabelTrigger, label: &str) -> Self {
        self.labels
            .entry(trigger)
            .or_default()
            .push(LabelModification::Remove(label.to_string()));
        self
    }

    pub fn create(self) -> RepositoryConfig {
        let RepoConfig { timeout, labels } = self.build().unwrap();
        RepositoryConfig { timeout, labels }
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Permissions {
    #[builder(field(ty = "HashSet<UserId>"))]
    review_users: HashSet<UserId>,
    #[builder(field(ty = "HashSet<UserId>"))]
    try_users: HashSet<UserId>,
}

impl PermissionsBuilder {
    pub fn create(self) -> UserPermissions {
        let Permissions {
            review_users,
            try_users,
        } = self.build().unwrap();
        UserPermissions::new(review_users, try_users)
    }

    pub fn add_default_users(mut self) -> Self {
        self.review_users.insert(default_user().id);
        self.try_users.insert(default_user().id);
        self
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Client {
    #[builder(default = "default_repo_name()")]
    name: GithubRepoName,
    #[builder(default = "PermissionsBuilder::default().add_default_users()")]
    permissions: PermissionsBuilder,
    #[builder(default)]
    config: RepoConfigBuilder,
    #[builder(default)]
    db: Option<MockedDBClient>,
}

impl ClientBuilder {
    pub async fn create_state(self) -> TestBorsState {
        let Client {
            name,
            permissions,
            config,
            db,
        } = self.build().unwrap();

        let mut branch_history = HashMap::default();
        let default_base_branch = default_base_branch();
        branch_history.insert(default_base_branch.name, vec![default_base_branch.sha]);
        let db = db.unwrap_or(create_test_db().await);

        let client = Arc::new(TestRepositoryClient {
            comments: Default::default(),
            name: name.clone(),
            merge_branches_fn: Mutex::new(Box::new(|| Ok(CommitSha(default_merge_sha())))),
            get_pr_fn: Mutex::new(Box::new(|pr| {
                Ok(PRBuilder::default().number(pr.0).create())
            })),
            check_suites: Default::default(),
            cancelled_workflows: Default::default(),
            added_labels: Default::default(),
            removed_labels: Default::default(),
            branch_history: Mutex::new(branch_history),
        });

        let repo_state = RepositoryState {
            repository: name.clone(),
            client: client.clone(),
            permissions: ArcSwap::new(Arc::new(permissions.create())),
            config: ArcSwap::new(Arc::new(config.create())),
        };
        let mut repos = HashMap::new();
        repos.insert(name.clone(), Arc::new(repo_state));
        TestBorsState {
            repos: Arc::new(repos),
            db: Arc::new(db),
            default_client: client,
        }
    }
}

pub struct TestRepositoryClient {
    name: GithubRepoName,
    comments: Mutex<HashMap<u64, Vec<String>>>,
    merge_branches_fn: Mutex<Box<dyn Fn() -> Result<CommitSha, MergeError> + Send + Sync>>,
    get_pr_fn: Mutex<Box<dyn Fn(PullRequestNumber) -> anyhow::Result<PullRequest> + Send + Sync>>,
    check_suites: Mutex<HashMap<String, Vec<CheckSuite>>>,
    cancelled_workflows: Mutex<HashSet<u64>>,
    added_labels: Mutex<HashMap<u64, Vec<String>>>,
    removed_labels: Mutex<HashMap<u64, Vec<String>>>,
    // Branch name -> history of SHAs
    branch_history: Mutex<HashMap<String, Vec<CommitSha>>>,
}

impl TestRepositoryClient {
    // Getters
    pub fn get_comment(&self, pr_number: u64, comment_index: usize) -> String {
        self.comments.lock().unwrap().get(&pr_number).unwrap()[comment_index].clone()
    }

    pub fn get_last_comment(&self, pr_number: u64) -> String {
        self.comments
            .lock()
            .unwrap()
            .get(&pr_number)
            .unwrap()
            .last()
            .unwrap()
            .clone()
    }

    // Setters
    pub fn set_checks(&self, commit: &str, checks: &[CheckSuite]) {
        self.check_suites
            .lock()
            .unwrap()
            .insert(commit.to_string(), checks.to_vec());
    }

    pub fn set_get_pr_fn<
        F: Fn(PullRequestNumber) -> anyhow::Result<PullRequest> + Send + Sync + 'static,
    >(
        &self,
        f: F,
    ) {
        *self.get_pr_fn.lock().unwrap() = Box::new(f);
    }

    pub fn set_merge_branches_fn<
        F: Fn() -> Result<CommitSha, MergeError> + Send + Sync + 'static,
    >(
        &self,
        f: F,
    ) {
        *self.merge_branches_fn.lock().unwrap() = Box::new(f);
    }

    // Checks
    pub fn check_comments(&self, pr_number: u64, comments: &[&str]) {
        assert_eq!(
            self.comments
                .lock()
                .unwrap()
                .get(&pr_number)
                .cloned()
                .unwrap_or_default(),
            comments
                .iter()
                .map(|&s| String::from(s))
                .collect::<Vec<_>>()
        );
    }
    pub fn check_comment_count(&self, pr_number: u64, count: usize) {
        assert_eq!(
            self.comments
                .lock()
                .unwrap()
                .get(&pr_number)
                .cloned()
                .unwrap_or_default()
                .len(),
            count
        );
    }

    pub fn check_added_labels(&self, pr: u64, added: &[&str]) -> &Self {
        assert_eq!(self.added_labels.lock().unwrap()[&pr], added);
        self
    }
    pub fn check_removed_labels(&self, pr: u64, removed: &[&str]) -> &Self {
        assert_eq!(self.removed_labels.lock().unwrap()[&pr], removed);
        self
    }

    pub fn check_cancelled_workflows(&self, cancelled: &[u64]) {
        let set = cancelled.iter().copied().collect::<HashSet<_>>();
        assert_eq!(*self.cancelled_workflows.lock().unwrap(), set);
    }

    pub fn check_branch_history(&self, branch: &str, sha: &[&str]) {
        let branch_history = self.branch_history.lock().unwrap();
        let history = branch_history
            .get(branch)
            .unwrap_or_else(|| panic!("Branch {branch} not found"));
        assert_eq!(
            history,
            &sha.into_iter()
                .map(|s| CommitSha(s.to_string()))
                .collect::<Vec<_>>()
        );
    }

    pub fn add_branch_sha(&self, branch: &str, sha: &str) {
        self.branch_history
            .lock()
            .unwrap()
            .entry(branch.to_string())
            .or_default()
            .push(CommitSha(sha.to_string()));
    }
}

#[async_trait]
impl RepositoryClient for Arc<TestRepositoryClient> {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn is_comment_internal(&self, comment: &PullRequestComment) -> anyhow::Result<bool> {
        Ok(comment.author == test_bot_user())
    }

    async fn get_branch_sha(&self, name: &str) -> anyhow::Result<CommitSha> {
        let sha = self
            .branch_history
            .lock()
            .unwrap()
            .get(name)
            .and_then(|history| history.last().cloned());
        sha.ok_or(anyhow::anyhow!("Branch {name} not found"))
    }

    async fn get_pull_request(&self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        (self.get_pr_fn.lock().unwrap())(pr)
    }

    async fn post_comment(&self, pr: PullRequestNumber, comment: Comment) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .entry(pr.0)
            .or_default()
            .push(comment.render().to_string());
        Ok(())
    }

    async fn set_branch_to_sha(&self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        self.add_branch_sha(branch, &sha.0);
        Ok(())
    }

    async fn merge_branches(
        &self,
        base: &str,
        _head: &CommitSha,
        _commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        let res = (self.merge_branches_fn.lock().unwrap())();
        if let Ok(ref sha) = res {
            self.add_branch_sha(base, &sha.0);
        }
        res
    }

    async fn get_check_suites_for_commit(
        &self,
        _branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>> {
        Ok(self
            .check_suites
            .lock()
            .unwrap()
            .get(&sha.0)
            .cloned()
            .unwrap_or_default())
    }

    async fn cancel_workflows(&self, run_ids: &[RunId]) -> anyhow::Result<()> {
        self.cancelled_workflows
            .lock()
            .unwrap()
            .extend(run_ids.into_iter().map(|id| id.0));
        Ok(())
    }

    async fn add_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        self.added_labels
            .lock()
            .unwrap()
            .entry(pr.0)
            .or_default()
            .extend(labels.to_vec());
        Ok(())
    }

    async fn remove_labels(&self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        self.removed_labels
            .lock()
            .unwrap()
            .entry(pr.0)
            .or_default()
            .extend(labels.to_vec());
        Ok(())
    }

    async fn load_config(&self) -> anyhow::Result<RepositoryConfig> {
        Ok(RepoConfigBuilder::default().create())
    }

    fn get_workflow_url(&self, run_id: RunId) -> String {
        let mut url = Url::parse("https://github.com").expect("Cannot parse base GitHub URL");
        url.set_path(
            format!(
                "{}/{}/actions/runs/{}",
                self.repository().owner(),
                self.repository().name(),
                run_id
            )
            .as_str(),
        );
        url.to_string()
    }
}
