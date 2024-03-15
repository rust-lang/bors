use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::string::ToString;
use std::time::Duration;

use crate::config::RepositoryConfig;
use axum::async_trait;
use derive_builder::Builder;
use octocrab::models::RunId;

use super::permissions::AllPermissions;
use crate::bors::event::{
    BorsEvent, CheckSuiteCompleted, PullRequestComment, WorkflowCompleted, WorkflowStarted,
};
use crate::bors::{handle_bors_event, BorsContext, CheckSuite, CommandParser, RepositoryState};
use crate::bors::{BorsState, RepositoryClient};
use crate::database::{DbClient, SeaORMClient, WorkflowStatus};
use crate::github::{
    CommitSha, GithubRepoName, GithubUser, LabelModification, LabelTrigger, PullRequest,
};
use crate::github::{MergeError, PullRequestNumber};
use crate::permissions::PermissionResolver;
use crate::tests::database::create_test_db;
use crate::tests::event::{
    CheckSuiteCompletedBuilder, WorkflowCompletedBuilder, WorkflowStartedBuilder,
};
use crate::tests::github::{default_base_branch, PRBuilder};

pub fn test_bot_user() -> GithubUser {
    GithubUser {
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
        handle_bors_event(
            event,
            self,
            &BorsContext::new(CommandParser::new("@bors".to_string())),
        )
        .await
        .unwrap();
    }

    pub async fn comment<T: Into<PullRequestComment>>(&mut self, comment: T) {
        self.event(BorsEvent::Comment(comment.into())).await;
    }

    pub async fn workflow_started<T: Into<WorkflowStarted>>(&mut self, payload: T) {
        self.event(BorsEvent::WorkflowStarted(payload.into())).await;
    }

    pub async fn workflow_completed<T: Into<WorkflowCompleted>>(&mut self, payload: T) {
        self.event(BorsEvent::WorkflowCompleted(payload.into()))
            .await;
    }

    pub async fn check_suite_completed<T: Into<CheckSuiteCompleted>>(&mut self, payload: T) {
        self.event(BorsEvent::CheckSuiteCompleted(payload.into()))
            .await;
    }

    pub async fn refresh(&mut self) {
        self.event(BorsEvent::Refresh).await;
    }

    pub async fn perform_workflow_events(
        &mut self,
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

    fn get_all_repos_mut(
        &mut self,
    ) -> (
        Vec<&mut RepositoryState<TestRepositoryClient>>,
        &mut dyn DbClient,
    ) {
        (
            self.repos.values_mut().collect(),
            (&mut self.db) as &mut dyn DbClient,
        )
    }

    fn reload_repositories(&mut self) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + '_>> {
        Box::pin(async move { Ok(()) })
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
pub struct Client {
    #[builder(default)]
    name: Option<GithubRepoName>,
    #[builder(default = "Box::new(AllPermissions)")]
    permission_resolver: Box<dyn PermissionResolver>,
    #[builder(default)]
    config: RepoConfigBuilder,
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

        let mut branch_history = HashMap::default();
        let default_base_branch = default_base_branch();
        branch_history.insert(default_base_branch.name, vec![default_base_branch.sha]);

        let name = name.unwrap_or_else(default_repo_name);
        RepositoryState {
            repository: name.clone(),
            client: TestRepositoryClient {
                comments: Default::default(),
                name,
                merge_branches_fn: Box::new(|| Ok(CommitSha(default_merge_sha()))),
                get_pr_fn: Box::new(move |pr| Ok(PRBuilder::default().number(pr.0).create())),
                check_suites: Default::default(),
                cancelled_workflows: Default::default(),
                added_labels: Default::default(),
                removed_labels: Default::default(),
                branch_history,
            },
            permissions_resolver: permission_resolver,
            config: config.create(),
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
    pub cancelled_workflows: HashSet<u64>,
    added_labels: HashMap<u64, Vec<String>>,
    removed_labels: HashMap<u64, Vec<String>>,
    // Branch name -> history of SHAs
    branch_history: HashMap<String, Vec<CommitSha>>,
}

impl TestRepositoryClient {
    // Getters
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

    // Setters
    pub fn set_checks(&mut self, commit: &str, checks: &[CheckSuite]) {
        self.check_suites
            .insert(commit.to_string(), checks.to_vec());
    }

    // Checks
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

    pub fn check_added_labels(&self, pr: u64, added: &[&str]) -> &Self {
        assert_eq!(self.added_labels[&pr], added);
        self
    }
    pub fn check_removed_labels(&self, pr: u64, removed: &[&str]) -> &Self {
        assert_eq!(self.removed_labels[&pr], removed);
        self
    }

    pub fn check_cancelled_workflows(&self, cancelled: &[u64]) {
        let set = cancelled.iter().copied().collect::<HashSet<_>>();
        assert_eq!(self.cancelled_workflows, set);
    }

    pub fn check_branch_history(&self, branch: &str, sha: &[&str]) {
        let history = self
            .branch_history
            .get(branch)
            .unwrap_or_else(|| panic!("Branch {branch} not found"));
        assert_eq!(
            history,
            &sha.into_iter()
                .map(|s| CommitSha(s.to_string()))
                .collect::<Vec<_>>()
        );
    }

    pub fn add_branch_sha(&mut self, branch: &str, sha: &str) {
        self.branch_history
            .entry(branch.to_string())
            .or_default()
            .push(CommitSha(sha.to_string()));
    }
}

#[async_trait]
impl RepositoryClient for TestRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn get_branch_sha(&mut self, name: &str) -> anyhow::Result<CommitSha> {
        let sha = self
            .branch_history
            .get(name)
            .and_then(|history| history.last().cloned());
        sha.ok_or(anyhow::anyhow!("Branch {name} not found"))
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

    async fn set_branch_to_sha(&mut self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        self.add_branch_sha(branch, &sha.0);
        Ok(())
    }

    async fn merge_branches(
        &mut self,
        base: &str,
        _head: &CommitSha,
        _commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        let res = (self.merge_branches_fn)();
        if let Ok(ref sha) = res {
            self.add_branch_sha(base, &sha.0);
        }
        res
    }

    async fn get_check_suites_for_commit(
        &mut self,
        _branch: &str,
        sha: &CommitSha,
    ) -> anyhow::Result<Vec<CheckSuite>> {
        Ok(self.check_suites.get(&sha.0).cloned().unwrap_or_default())
    }

    async fn cancel_workflows(&mut self, run_ids: Vec<RunId>) -> anyhow::Result<()> {
        self.cancelled_workflows
            .extend(run_ids.into_iter().map(|id| id.0));
        Ok(())
    }

    async fn add_labels(&mut self, pr: PullRequestNumber, labels: &[String]) -> anyhow::Result<()> {
        self.added_labels
            .entry(pr.0)
            .or_default()
            .extend(labels.to_vec());
        Ok(())
    }

    async fn remove_labels(
        &mut self,
        pr: PullRequestNumber,
        labels: &[String],
    ) -> anyhow::Result<()> {
        self.removed_labels
            .entry(pr.0)
            .or_default()
            .extend(labels.to_vec());
        Ok(())
    }
}
