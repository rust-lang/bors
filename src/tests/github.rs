use crate::OAuthConfig;
use crate::bors::PullRequestStatus;
use crate::database::WorkflowStatus;
use crate::github::api::client::HideCommentReason;
use crate::github::{GithubRepoName, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::tests::{AUTO_BRANCH, TRY_BRANCH};
use chrono::{DateTime, Utc};
use octocrab::models::pulls::MergeableState;
use octocrab::models::{CheckSuiteId, JobId, RunId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::{Receiver, Sender};

/// Represents the state of GitHub.
pub struct GitHub {
    pub(super) repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
    comments: HashMap<String, Comment>,
    users: HashMap<String, User>,
    pub oauth_config: OAuthConfig,
    workflow_run_id_counter: u64,
}

impl GitHub {
    /// Creates a new GitHub where the default PR author has no permissions.
    pub fn unauthorized_pr_author() -> Self {
        let state = Self::default();
        state
            .default_repo()
            .lock()
            .permissions
            .users
            .insert(User::default_pr_author(), vec![]);
        state
    }

    pub fn with_default_config(self, config: &str) -> Self {
        self.default_repo().lock().config = config.to_string();
        self
    }

    pub fn add_user(&mut self, user: User) {
        assert!(
            !self
                .users
                .values()
                .map(|u| u.github_id)
                .any(|id| id == user.github_id)
        );
        assert!(self.users.insert(user.name.clone(), user).is_none());
    }

    pub fn default_repo(&self) -> Arc<Mutex<Repo>> {
        self.get_repo(default_repo_name())
    }

    pub fn get_repo<Id: Into<RepoIdentifier>>(&self, id: Id) -> Arc<Mutex<Repo>> {
        let id = id.into();
        self.repos.get(&id.0).expect("Repo not found").clone()
    }

    pub fn add_repo(&mut self, repo: Repo) {
        assert!(
            self.repos
                .insert(repo.full_name(), Arc::new(Mutex::new(repo)))
                .is_none()
        );
    }

    pub fn with_repo(mut self, repo: Repo) -> Self {
        self.add_repo(repo);
        self
    }

    pub fn get_repo_by_run_id(&self, run_id: RunId) -> Arc<Mutex<Repo>> {
        self.repos
            .values()
            .find(|repo| repo.lock().workflow_runs.iter().any(|w| w.run_id == run_id))
            .unwrap()
            .clone()
    }

    pub fn modify_comment<F: FnOnce(&mut Comment)>(&mut self, node_id: &str, func: F) {
        func(self.comments.get_mut(node_id).unwrap());
    }

    pub fn check_sha_history<Id: Into<RepoIdentifier>>(
        &self,
        repo: Id,
        branch: &str,
        expected_shas: &[&str],
    ) {
        let actual_shas = self
            .get_repo(repo)
            .lock()
            .get_branch_by_name(branch)
            .expect("Branch not found")
            .get_sha_history();
        let actual_shas: Vec<&str> = actual_shas.iter().map(|s| s.as_str()).collect();
        assert_eq!(actual_shas, expected_shas);
    }

    pub fn check_cancelled_workflows(&self, repo: GithubRepoName, expected_run_ids: &[RunId]) {
        let mut workflows = self
            .get_repo(&repo)
            .lock()
            .workflows_cancelled_by_bors
            .clone();
        workflows.sort();

        let mut expected = expected_run_ids.to_vec();
        expected.sort();

        assert_eq!(workflows, expected);
    }

    /// This function is an important synchronization point, which is used to wait for
    /// events to arrive from the bors service.
    /// As such, it has to be written carefully to avoid holding GH/repo locks that are also
    /// acquired by dynamic HTTP mock handlers.
    pub async fn get_next_comment<Id: Into<PrIdentifier>>(
        state: Arc<Mutex<Self>>,
        id: Id,
    ) -> anyhow::Result<Comment> {
        use std::fmt::Write;

        let id = id.into();
        // We need to avoid holding the GH state and repo lock here, otherwise the mocking code
        // could not lock the repo and send the comment (or other information) to a PR.
        let comment_rx = {
            let mut gh_state = state.lock();
            let repo = gh_state
                .repos
                .get_mut(&id.repo)
                .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
            let repo = repo.lock();
            let pr = repo
                .pull_requests
                .get(&id.number)
                .expect("Pull request not found");
            pr.comment_queue_rx.clone()
        };
        let mut guard = comment_rx.lock().await;

        // Timeout individual comment reads to give better error messages than if the whole test
        // times out.
        let comment = match tokio::time::timeout(Duration::from_secs(2), guard.recv()).await {
            Ok(comment) => comment,
            Err(_) => {
                let mut comment_history = String::new();

                let mut gh_state = state.lock();
                let repo = gh_state
                    .repos
                    .get_mut(&id.repo)
                    .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
                let repo = repo.lock();
                let pr = repo
                    .pull_requests
                    .get(&id.number)
                    .expect("Pull request not found");
                for comment in &pr.comment_history {
                    writeln!(
                        comment_history,
                        "[{}]: {}",
                        comment.author.name, comment.content
                    )
                    .unwrap();
                }

                return Err(anyhow::anyhow!(
                    "Timed out waiting for a comment on {id}. Comment history:\n{comment_history}"
                ));
            }
        };
        let comment = comment.expect("Channel was closed while waiting for a comment");
        let comment = match comment {
            CommentMsg::Comment(comment) => comment,
            CommentMsg::Close => unreachable!(),
        };

        {
            let mut gh_state = state.lock();
            {
                let repo = gh_state
                    .repos
                    .get_mut(&id.repo)
                    .unwrap_or_else(|| panic!("Repository `{}` not found", id.repo));
                let mut repo = repo.lock();
                let pr = repo
                    .pull_requests
                    .get_mut(&id.number)
                    .expect("Pull request not found");
                pr.add_comment_to_history(comment.clone());
            }

            assert_eq!(
                gh_state.comments.insert(
                    comment.node_id.clone().expect("Comment without node ID"),
                    comment.clone()
                ),
                None,
                "Duplicated comment {comment:?}"
            );
        }

        eprintln!(
            "Received comment on {}#{}: {}",
            id.repo, id.number, comment.content
        );
        Ok(comment)
    }

    pub fn get_comment_by_node_id(&self, node_id: &str) -> Option<&Comment> {
        self.comments.get(node_id)
    }

    pub fn check_comment_was_hidden(&self, node_id: &str, reason: HideCommentReason) {
        let comment = self
            .comments
            .get(node_id)
            .expect("Comment with node_id {node_id} was not modified through GraphQL");
        assert_eq!(
            comment.hide_reason,
            Some(reason),
            "Comment {comment:?} was not hidden with reason {reason:?}."
        );
    }

    pub fn new_workflow(&mut self, repo: &GithubRepoName, branch: &str) -> RunId {
        let repo = self.get_repo(repo);
        let mut repo = repo.lock();
        let branch = repo.get_branch_by_name(branch).expect("Branch not found");
        self.workflow_run_id_counter += 1;
        let run_id = RunId(self.workflow_run_id_counter);
        let workflow = WorkflowRun::new(run_id, branch);
        repo.workflow_runs.push(workflow);
        run_id
    }
}

/// Represents the default GitHub state for tests.
/// It uses a basic configuration that might be also encountered on a real repository.
///
/// It contains a single pull request by default, [PullRequest::default], and a single
/// branch called `main`.
impl Default for GitHub {
    fn default() -> Self {
        let mut gh = Self {
            repos: HashMap::default(),
            comments: Default::default(),
            users: Default::default(),
            oauth_config: default_oauth_config(),
            workflow_run_id_counter: 0,
        };

        let config = r#"
timeout = 3600
merge_queue_enabled = true

# Set labels on PR approvals
[labels]
approved = ["+approved"]
"#
        .to_string();

        gh.add_user(User::default_pr_author());
        gh.add_user(User::try_user());
        gh.add_user(User::reviewer());
        gh.add_user(User::bors_bot());
        gh.add_user(User::unprivileged());

        let repo_name = default_repo_name();
        let org_user = User::new(1000001, repo_name.owner());
        gh.add_user(org_user.clone());

        let mut users = HashMap::default();
        users.insert(
            User::default_pr_author(),
            vec![PermissionType::Try, PermissionType::Review],
        );
        users.insert(User::try_user(), vec![PermissionType::Try]);
        users.insert(
            User::reviewer(),
            vec![PermissionType::Try, PermissionType::Review],
        );

        let mut repo = Repo::new(org_user.clone(), repo_name.name()).with_pr(PullRequest::new(
            repo_name,
            default_pr_number(),
            User::default_pr_author(),
        ));
        repo.config = config;
        repo.permissions = Permissions { users };
        gh.add_repo(repo);
        gh
    }
}

#[derive(Eq, PartialEq, Hash, Clone, Debug)]
pub struct User {
    pub github_id: u64,
    pub name: String,
}

impl User {
    pub fn new(id: u64, name: &str) -> Self {
        Self {
            github_id: id,
            name: name.to_string(),
        }
    }

    /// The user that creates a pull request by default.
    pub fn default_pr_author() -> Self {
        Self::new(101, "default-user")
    }

    pub fn bors_bot() -> Self {
        Self::new(102, "bors-bot")
    }

    /// User that should not have any privileges.
    pub fn unprivileged() -> Self {
        Self::new(103, "unprivileged-user")
    }

    /// User that should have `try` privileges.
    pub fn try_user() -> Self {
        Self::new(104, "user-with-try-privileges")
    }

    /// User that should have `try` and `review` privileges.
    pub fn reviewer() -> Self {
        Self::new(105, "reviewer")
    }
}

#[derive(Clone)]
pub struct Repo {
    name: String,
    owner: User,
    pub permissions: Permissions,
    pub config: String,
    branches: Vec<Branch>,
    commit_messages: HashMap<String, String>,
    workflows_cancelled_by_bors: Vec<RunId>,
    pub workflow_cancel_error: bool,
    /// All workflows that we know about from the side of the test.
    workflow_runs: Vec<WorkflowRun>,
    pub pull_requests: HashMap<u64, PullRequest>,
    pub check_runs: Vec<CheckRunData>,
    /// Cause pull request fetch to fail.
    pub pull_request_error: bool,
    /// Push error failure/success behaviour.
    pub push_behaviour: BranchPushBehaviour,
    pub pr_push_counter: u64,
    pub fork: bool,
}

impl Repo {
    pub fn new(owner: User, name: &str) -> Self {
        Self {
            name: name.to_string(),
            permissions: Permissions::empty(),
            config: String::new(),
            owner,
            pull_requests: Default::default(),
            branches: vec![Branch::default()],
            commit_messages: Default::default(),
            workflows_cancelled_by_bors: vec![],
            workflow_cancel_error: false,
            workflow_runs: vec![],
            pull_request_error: false,
            pr_push_counter: 0,
            check_runs: vec![],
            push_behaviour: BranchPushBehaviour::default(),
            fork: false,
        }
    }

    pub fn full_name(&self) -> GithubRepoName {
        GithubRepoName::new(&self.owner.name, &self.name)
    }

    pub fn owner(&self) -> &User {
        &self.owner
    }

    pub fn branches(&self) -> &[Branch] {
        &self.branches
    }

    pub fn with_user_perms(mut self, user: User, permissions: &[PermissionType]) -> Self {
        self.permissions.users.insert(user, permissions.to_vec());
        self
    }

    // TODO: refactor to now allow anyone to add PR from the outside
    pub fn with_pr(mut self, pr: PullRequest) -> Self {
        assert!(self.pull_requests.insert(pr.number.0, pr).is_none());
        self
    }

    pub fn new_pr(&mut self, author: User) -> &mut PullRequest {
        let number = self.pull_requests.keys().copied().max().unwrap_or(0) + 1;
        let pr = PullRequest::new(self.full_name(), number, author);
        assert!(self.pull_requests.insert(pr.number.0, pr).is_none());
        self.pull_requests.get_mut(&number).unwrap()
    }

    pub fn get_pr(&self, pr: u64) -> &PullRequest {
        self.pull_requests.get(&pr).unwrap()
    }

    pub fn get_pr_mut(&mut self, pr: u64) -> &mut PullRequest {
        self.pull_requests.get_mut(&pr).unwrap()
    }

    pub fn add_branch(&mut self, branch: Branch) {
        assert!(self.get_branch_by_name(&branch.name).is_none());
        self.branches.push(branch);
    }

    pub fn try_branch(&self) -> Branch {
        self.branches
            .iter()
            .find(|b| b.name == TRY_BRANCH)
            .expect("Try branch not found")
            .clone()
    }

    pub fn auto_branch(&self) -> Branch {
        self.branches
            .iter()
            .find(|b| b.name == AUTO_BRANCH)
            .expect("Auto branch not found")
            .clone()
    }

    pub fn get_branch_by_name(&mut self, name: &str) -> Option<&mut Branch> {
        self.branches.iter_mut().find(|b| b.name == name)
    }

    pub fn add_cancelled_workflow(&mut self, run_id: RunId) {
        self.workflows_cancelled_by_bors.push(run_id);
        self.get_workflow_mut(run_id).status = WorkflowStatus::Failure;
    }

    pub fn add_check_run(&mut self, check_run: CheckRunData) {
        self.check_runs.push(check_run);
    }

    pub fn update_check_run(
        &mut self,
        check_run_id: u64,
        status: String,
        conclusion: Option<String>,
    ) {
        let check_run = self.check_runs.get_mut(check_run_id as usize).unwrap();
        check_run.status = status;
        check_run.conclusion = conclusion;
    }

    pub fn get_workflow_mut(&mut self, run_id: RunId) -> &mut WorkflowRun {
        self.workflow_runs
            .iter_mut()
            .find(|w| w.run_id == run_id)
            .unwrap()
    }

    pub fn get_next_pr_push_counter(&mut self) -> u64 {
        self.pr_push_counter += 1;
        self.pr_push_counter
    }

    pub fn get_commit_message(&self, sha: &str) -> String {
        self.commit_messages
            .get(sha)
            .expect("Commit message not found")
            .clone()
    }

    pub fn set_commit_message(&mut self, sha: &str, message: &str) {
        self.commit_messages
            .insert(sha.to_string(), message.to_string());
    }

    pub fn find_workflow(&self, id: RunId) -> Option<WorkflowRun> {
        self.workflow_runs.iter().find(|w| w.run_id == id).cloned()
    }

    pub fn find_workflows_by_check_suite_id(&self, id: CheckSuiteId) -> Vec<WorkflowRun> {
        self.workflow_runs
            .iter()
            .filter(|w| w.check_suite_id == id)
            .cloned()
            .collect()
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("rust-lang", "borstest")
}

pub fn default_pr_number() -> u64 {
    1
}

pub fn default_oauth_config() -> OAuthConfig {
    OAuthConfig::new(test_oauth_client_id(), test_oauth_client_secret())
}

fn test_oauth_client_id() -> String {
    "OAUTH_CLIENT_ID".to_string()
}

fn test_oauth_client_secret() -> String {
    "OAUTH_CLIENT_SECRET".to_string()
}

#[derive(Clone, Debug, PartialEq)]
pub struct RepoIdentifier(pub GithubRepoName);

impl From<()> for RepoIdentifier {
    fn from(_: ()) -> Self {
        Self(default_repo_name())
    }
}

impl From<GithubRepoName> for RepoIdentifier {
    fn from(value: GithubRepoName) -> Self {
        Self(value)
    }
}
impl<'a> From<&'a GithubRepoName> for RepoIdentifier {
    fn from(value: &'a GithubRepoName) -> Self {
        Self(value.clone())
    }
}

/// Helper struct for uniquely identifying a pull request.
/// Used to reduce boilerplate in tests.
///
/// Can be created from:
/// - `()`, which uses the default repo and default PR number.
/// - A PR number, which uses the default repository.
/// - A tuple (<repo-name>, <PR number>).
#[derive(Clone, Debug, PartialEq)]
pub struct PrIdentifier {
    pub repo: GithubRepoName,
    pub number: u64,
}

impl Display for PrIdentifier {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let PrIdentifier { repo, number } = self;
        f.write_fmt(format_args!("{repo}#{number}"))
    }
}

impl Default for PrIdentifier {
    fn default() -> Self {
        Self {
            repo: default_repo_name(),
            number: default_pr_number(),
        }
    }
}

impl From<u64> for PrIdentifier {
    fn from(number: u64) -> Self {
        Self {
            repo: default_repo_name(),
            number,
        }
    }
}

impl From<PullRequestNumber> for PrIdentifier {
    fn from(value: PullRequestNumber) -> Self {
        value.0.into()
    }
}

impl From<(GithubRepoName, u64)> for PrIdentifier {
    fn from(value: (GithubRepoName, u64)) -> Self {
        Self {
            repo: value.0,
            number: value.1,
        }
    }
}

impl From<()> for PrIdentifier {
    fn from(_: ()) -> Self {
        Self {
            repo: default_repo_name(),
            number: default_pr_number(),
        }
    }
}

impl From<PullRequest> for PrIdentifier {
    fn from(pr: PullRequest) -> Self {
        pr.id()
    }
}

#[derive(Clone, Debug)]
pub struct PullRequest {
    pub number: PullRequestNumber,
    pub repo: GithubRepoName,
    pub labels_added_by_bors: Vec<String>,
    pub labels_removed_by_bors: Vec<String>,
    pub comment_counter: u64,
    pub head_sha: String,
    pub author: User,
    pub base_branch: Branch,
    pub mergeable_state: MergeableState,
    pub(super) status: PullRequestStatus,
    pub(super) merged_at: Option<DateTime<Utc>>,
    pub(super) closed_at: Option<DateTime<Utc>>,
    pub assignees: Vec<User>,
    pub description: String,
    pub title: String,
    pub labels: Vec<String>,
    pub comment_queue_tx: Sender<CommentMsg>,
    pub comment_queue_rx: Arc<tokio::sync::Mutex<Receiver<CommentMsg>>>,
    pub comment_history: Vec<Comment>,
}

impl PullRequest {
    pub fn new(repo: GithubRepoName, number: u64, author: User) -> Self {
        // The size of the buffer is load-bearing, if we receive too many comments, the test harness
        // could deadlock.
        let (comment_queue_tx, comment_queue_rx) = tokio::sync::mpsc::channel(100);
        Self {
            number: PullRequestNumber(number),
            repo,
            labels_added_by_bors: Vec::new(),
            labels_removed_by_bors: Vec::new(),
            comment_counter: 0,
            head_sha: format!("pr-{number}-sha"),
            author,
            base_branch: Branch::default(),
            mergeable_state: MergeableState::Clean,
            status: PullRequestStatus::Open,
            merged_at: None,
            closed_at: None,
            assignees: Vec::new(),
            description: format!("Description of PR {number}"),
            title: format!("Title of PR {number}"),
            labels: Vec::new(),
            comment_queue_tx,
            comment_queue_rx: Arc::new(tokio::sync::Mutex::new(comment_queue_rx)),
            comment_history: Vec::new(),
        }
    }

    pub fn id(&self) -> PrIdentifier {
        PrIdentifier {
            repo: self.repo.clone(),
            number: self.number.0,
        }
    }

    /// Return a numeric ID and a node ID for the next comment to be created.
    pub fn next_comment_ids(&mut self) -> (u64, String) {
        self.comment_counter += 1;
        (
            self.comment_counter,
            format!(
                "comment-{}_{}-{}",
                self.repo, self.number, self.comment_counter
            ),
        )
    }

    pub fn merge_pr(&mut self) {
        self.merged_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Merged;
    }

    pub fn close_pr(&mut self) {
        self.closed_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Closed;
    }

    pub fn reopen_pr(&mut self) {
        self.closed_at = None;
        self.status = PullRequestStatus::Open;
    }

    pub fn ready_for_review(&mut self) {
        self.status = PullRequestStatus::Open;
    }

    pub fn convert_to_draft(&mut self) {
        self.status = PullRequestStatus::Draft;
    }

    pub fn add_comment_to_history(&mut self, comment: Comment) {
        self.comment_history.push(comment);
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    sha_history: Vec<String>,
    pub merge_counter: u64,
    pub merge_conflict: bool,
}

impl Branch {
    pub fn new(name: &str, sha: &str) -> Self {
        Self {
            name: name.to_string(),
            sha: sha.to_string(),
            sha_history: vec![],
            merge_counter: 0,
            merge_conflict: false,
        }
    }

    pub fn get_name(&self) -> &str {
        &self.name
    }
    pub fn get_sha(&self) -> &str {
        &self.sha
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
        Self::new(default_branch_name(), default_branch_sha())
    }
}

#[derive(Clone, Debug)]
pub enum BranchPushError {
    Conflict,
    ValidationFailed,
    InternalServerError,
}

#[derive(Clone, Debug)]
pub struct BranchPushBehaviour {
    pub error: Option<(BranchPushError, NonZeroU64)>,
}

impl BranchPushBehaviour {
    pub fn success() -> Self {
        Self { error: None }
    }

    pub fn always_fail(error_type: BranchPushError) -> Self {
        Self {
            error: Some((error_type, NonZeroU64::new(u64::MAX).unwrap())),
        }
    }

    pub fn fail_n_times(error_type: BranchPushError, count: u64) -> Self {
        Self {
            error: NonZeroU64::new(count).map(|remaining| (error_type, remaining)),
        }
    }
}

impl Default for BranchPushBehaviour {
    fn default() -> Self {
        Self::success()
    }
}

pub fn default_branch_name() -> &'static str {
    "main"
}

pub fn default_branch_sha() -> &'static str {
    "main-sha1"
}

pub enum CommentMsg {
    Comment(Comment),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub pr_ident: PrIdentifier,
    pub author: User,
    pub content: String,
    pub id: Option<u64>,
    pub node_id: Option<String>,
    pub hide_reason: Option<HideCommentReason>,
}

impl Comment {
    pub fn new<Id: Into<PrIdentifier>>(id: Id, content: &str) -> Self {
        Self {
            pr_ident: id.into(),
            author: User::default_pr_author(),
            content: content.to_string(),
            id: None,
            node_id: None,
            hide_reason: None,
        }
    }

    pub fn with_author(self, author: User) -> Self {
        Self { author, ..self }
    }

    pub fn with_ids(self, id: u64, node_id: String) -> Self {
        Self {
            id: Some(id),
            node_id: Some(node_id),
            ..self
        }
    }
}

impl<'a> From<&'a str> for Comment {
    fn from(value: &'a str) -> Self {
        Comment::new(PrIdentifier::default(), value)
    }
}

#[derive(Clone)]
pub struct Permissions {
    pub users: HashMap<User, Vec<PermissionType>>,
}

impl Permissions {
    /// Empty permissions => no one has permissions.
    pub fn empty() -> Self {
        Self {
            users: HashMap::default(),
        }
    }
}

#[derive(Clone)]
pub struct WorkflowEvent {
    pub event: WorkflowEventKind,
    pub run_id: RunId,
}

impl WorkflowEvent {
    pub fn started(run_id: RunId) -> WorkflowEvent {
        Self {
            event: WorkflowEventKind::Started,
            run_id,
        }
    }
    pub fn success(run_id: RunId) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: "success".to_string(),
            },
            run_id,
        }
    }
    pub fn failure(run_id: RunId) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: "failure".to_string(),
            },
            run_id,
        }
    }
}

#[derive(Clone)]
pub enum WorkflowEventKind {
    Started,
    Completed { status: String },
}

#[derive(Clone)]
pub struct WorkflowJob {
    pub id: JobId,
    pub status: WorkflowStatus,
}

#[derive(Copy, Clone)]
pub enum TestWorkflowStatus {
    Success,
    Failure,
}

#[derive(Clone, Debug)]
pub struct CheckRunData {
    pub name: String,
    pub head_sha: String,
    pub status: String,
    pub conclusion: Option<String>,
    pub title: String,
    pub summary: String,
    pub text: String,
    pub external_id: String,
}

#[derive(Clone)]
pub struct WorkflowRun {
    name: String,
    run_id: RunId,
    check_suite_id: CheckSuiteId,
    head_branch: String,
    jobs: Vec<WorkflowJob>,
    head_sha: String,
    /// How long did the workflow run for?
    duration: Duration,
    status: WorkflowStatus,
}

impl WorkflowRun {
    fn new(run_id: RunId, branch: &Branch) -> Self {
        Self {
            status: WorkflowStatus::Pending,
            name: "Workflow1".to_string(),
            run_id,
            check_suite_id: CheckSuiteId(run_id.0),
            head_branch: branch.get_name().to_string(),
            jobs: vec![],
            head_sha: branch.get_sha().to_string(),
            duration: Duration::from_secs(3600),
        }
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn change_status(&mut self, status: WorkflowStatus) {
        self.status = status;
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn check_suite_id(&self) -> CheckSuiteId {
        self.check_suite_id
    }

    pub fn head_branch(&self) -> &str {
        &self.head_branch
    }

    pub fn jobs(&self) -> &[WorkflowJob] {
        &self.jobs
    }

    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }
    pub fn set_duration(&mut self, duration: Duration) {
        self.duration = duration;
    }

    pub fn status(&self) -> WorkflowStatus {
        self.status
    }

    pub fn add_job(&mut self, status: WorkflowStatus) {
        self.jobs.push(WorkflowJob {
            id: JobId(self.run_id.0 * 1000 + self.jobs.len() as u64),
            status,
        });
    }
}
