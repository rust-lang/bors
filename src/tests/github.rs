use crate::OAuthConfig;
use crate::bors::PullRequestStatus;
use crate::database::WorkflowStatus;
use crate::github::api::client::HideCommentReason;
use crate::github::{CommitSha, GithubRepoName, PullRequestNumber};
use crate::permissions::PermissionType;
use crate::tests::COMMENT_RECEIVE_TIMEOUT;
use chrono::{DateTime, Utc};
use http::StatusCode;
use octocrab::models::pulls::MergeableState;
use octocrab::models::workflows::Conclusion;
use octocrab::models::{CheckSuiteId, JobId, RunId};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::{Receiver, Sender};

/// Represents the state of GitHub.
pub struct GitHub {
    pub(super) repos: HashMap<GithubRepoName, Arc<Mutex<Repo>>>,
    comments: HashMap<String, Comment>,
    users: HashMap<String, User>,
    teams: HashSet<String>,
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

    pub fn append_to_default_config(self, config: &str) -> Self {
        self.default_repo()
            .lock()
            .config
            .push_str(&format!("\n{config}"));
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

    pub fn get_user(&self, name: &str) -> Option<&User> {
        self.users.get(name)
    }

    pub fn users(&self) -> Vec<User> {
        self.users.values().cloned().collect()
    }

    pub fn add_team(&mut self, name: &str) {
        self.teams.insert(name.to_string());
    }

    pub fn teams(&self) -> Vec<String> {
        self.teams.iter().cloned().collect()
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

    /// Get SHA history as a list of lines, for easier snapshot testing.
    pub fn get_sha_history<Id: Into<RepoIdentifier>>(&self, repo: Id, branch: &str) -> String {
        let actual_shas = self
            .get_repo(repo)
            .lock()
            .get_branch_by_name(branch)
            .expect("Branch not found")
            .get_commit_history()
            .into_iter()
            .map(|b| b.sha)
            .collect::<Vec<_>>();
        actual_shas.join("\n")
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
        let comment = match tokio::time::timeout(COMMENT_RECEIVE_TIMEOUT, guard.recv()).await {
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
            teams: Default::default(),
            oauth_config: default_oauth_config(),
            workflow_run_id_counter: 0,
        };

        let config = r#"
timeout = 3600
merge_queue_enabled = true
report_merge_conflicts = true
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

        let mut repo = Repo::new(org_user.clone(), repo_name.name());
        repo.add_pr(User::default_pr_author());
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

#[derive(Default)]
pub enum MergeBehavior {
    #[default]
    Succeed,
    /// Return HTTP status of the merge if it should fail immediately or `None` if it should
    /// continue.
    Custom(Box<dyn FnMut() -> Option<StatusCode> + Send + Sync>),
}

impl MergeBehavior {
    pub fn get_failure_status_code(&mut self) -> Option<StatusCode> {
        match self {
            MergeBehavior::Succeed => None,
            MergeBehavior::Custom(func) => func(),
        }
    }
}

pub struct Repo {
    name: String,
    owner: User,
    pub permissions: Permissions,
    pub config: String,
    branches: Vec<Branch>,
    commits: HashMap<CommitSha, Commit>,
    workflows_cancelled_by_bors: Vec<RunId>,
    pub workflow_cancel_error: bool,
    /// All workflows that we know about from the side of the test.
    workflow_runs: Vec<WorkflowRun>,
    pull_requests: HashMap<u64, PullRequest>,
    check_runs: Vec<CheckRunData>,
    /// Cause pull request fetch to fail.
    pub pull_request_error: bool,
    /// Push error failure/success behaviour.
    pub push_behaviour: BranchPushBehaviour,
    pub fork: bool,
    pub merge_behavior: MergeBehavior,
}

impl Repo {
    pub fn new(owner: User, name: &str) -> Self {
        let mut repo = Self {
            name: name.to_string(),
            permissions: Permissions::empty(),
            config: String::new(),
            owner,
            pull_requests: Default::default(),
            branches: vec![],
            commits: Default::default(),
            workflows_cancelled_by_bors: vec![],
            workflow_cancel_error: false,
            workflow_runs: vec![],
            pull_request_error: false,
            check_runs: vec![],
            push_behaviour: BranchPushBehaviour::default(),
            fork: false,
            merge_behavior: MergeBehavior::default(),
        };
        repo.add_branch(Branch::default());
        repo
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

    pub fn check_runs(&self) -> &[CheckRunData] {
        &self.check_runs
    }

    pub fn pulls(&self) -> &HashMap<u64, PullRequest> {
        &self.pull_requests
    }

    pub fn pulls_mut(&mut self) -> &mut HashMap<u64, PullRequest> {
        &mut self.pull_requests
    }

    pub fn with_user_perms(mut self, user: User, permissions: &[PermissionType]) -> Self {
        self.permissions.users.insert(user, permissions.to_vec());
        self
    }

    pub fn add_pr(&mut self, author: User) -> &mut PullRequest {
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
        for commit in &branch.commits {
            self.commits
                .entry(commit.commit_sha())
                .or_insert_with(|| commit.clone());
        }
        self.branches.push(branch);
    }

    pub fn get_branch_by_name(&mut self, name: &str) -> Option<&mut Branch> {
        if let Some(branch) = self.branches.iter_mut().find(|b| b.name == name) {
            Some(branch)
        } else {
            for pr in self.pull_requests.values_mut() {
                if pr.head_branch.name == name {
                    return Some(&mut pr.head_branch);
                }
            }
            None
        }
    }

    pub fn push_commit(&mut self, branch_name: &str, commit: Commit, force: bool) {
        self.create_commit(commit.clone());
        self.get_branch_by_name(branch_name)
            .expect("Pushing to a non-existing branch")
            .push_commit(commit, force);
    }

    pub fn create_commit(&mut self, commit: Commit) {
        if let Some(old) = self.commits.insert(commit.commit_sha(), commit.clone()) {
            assert_eq!(old, commit);
        }
    }

    pub fn get_commit_by_sha(&self, sha: &str) -> Commit {
        self.commits
            .get(&CommitSha(sha.to_owned()))
            .expect("Looking up non-existing commit SHA")
            .clone()
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

    pub fn find_workflow(&self, id: RunId) -> Option<WorkflowRun> {
        self.workflow_runs.iter().find(|w| w.run_id == id).cloned()
    }

    pub fn find_workflows_by_commit_sha(&self, sha: &str) -> Vec<WorkflowRun> {
        self.workflow_runs
            .iter()
            .filter(|w| w.head_sha == sha)
            .cloned()
            .collect()
    }
}

pub fn default_repo_name() -> GithubRepoName {
    GithubRepoName::new("rust-lang", "borstest")
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
            number: 1,
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
    pub(super) number: PullRequestNumber,
    pub(super) repo: GithubRepoName,
    pub(super) comment_counter: u64,
    pub(super) author: User,
    pub base_branch: Branch,
    pub(super) head_branch: Branch,
    pub mergeable_state: MergeableState,
    pub(super) status: PullRequestStatus,
    pub(super) merged_at: Option<DateTime<Utc>>,
    pub(super) closed_at: Option<DateTime<Utc>>,
    pub assignees: Vec<User>,
    pub description: String,
    pub title: String,
    pub labels: Vec<String>,
    /// Set to `Some` to specify that this is a PR from a forked repository.
    pub head_repository: Option<GithubRepoName>,
    pub(super) labels_added_by_bors: Vec<String>,
    pub(super) labels_removed_by_bors: Vec<String>,
    pub(super) comment_queue_tx: Sender<CommentMsg>,
    pub(super) comment_queue_rx: Arc<tokio::sync::Mutex<Receiver<CommentMsg>>>,
    pub(super) comment_history: Vec<Comment>,
    pub maintainers_can_modify: bool,
}

impl PullRequest {
    pub fn new(repo: GithubRepoName, number: u64, author: User) -> Self {
        // The size of the buffer is load-bearing, if we receive too many comments, the test harness
        // could deadlock.
        let (comment_queue_tx, comment_queue_rx) = tokio::sync::mpsc::channel(100);
        let head_branch = Branch::new(
            &format!("pr/{number}"),
            Commit::new(
                &format!("pr-{number}-sha"),
                &format!("initial PR#{number} commit"),
            )
            .with_author(GitUser {
                name: author.name.clone(),
                email: format!("{}@email.com", author.name),
            }),
        );
        Self {
            number: PullRequestNumber(number),
            repo,
            comment_counter: 0,
            head_branch,
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
            labels_added_by_bors: Vec::new(),
            labels_removed_by_bors: Vec::new(),
            head_repository: None,
            comment_queue_tx,
            comment_queue_rx: Arc::new(tokio::sync::Mutex::new(comment_queue_rx)),
            comment_history: Vec::new(),
            maintainers_can_modify: true,
        }
    }

    pub fn number(&self) -> PullRequestNumber {
        self.number
    }

    pub fn repo(&self) -> &GithubRepoName {
        &self.repo
    }

    pub fn head_sha(&self) -> String {
        self.head_branch.sha()
    }

    pub fn head_branch_copy(&self) -> Branch {
        self.head_branch.clone()
    }

    pub fn id(&self) -> PrIdentifier {
        PrIdentifier {
            repo: self.repo.clone(),
            number: self.number.0,
        }
    }

    pub fn reset_to_single_commit(&mut self, commit: Commit) {
        self.head_branch.push_commit(commit, true);
    }

    pub fn add_commits(&mut self, commits: Vec<Commit>) {
        for commit in &commits {
            assert!(!self.head_branch.commits.iter().any(|c| c.sha == commit.sha));
        }
        for commit in commits {
            self.head_branch.push_commit(commit, false);
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

    pub fn merge(&mut self) {
        self.merged_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Merged;
    }

    pub fn close(&mut self) {
        self.closed_at = Some(SystemTime::now().into());
        self.status = PullRequestStatus::Closed;
    }

    pub fn reopen(&mut self) {
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

    pub fn labels_added_by_bors(&self) -> &[String] {
        &self.labels_added_by_bors
    }

    pub fn labels_removed_by_bors(&self) -> &[String] {
        &self.labels_removed_by_bors
    }

    pub fn add_labels(&mut self, labels: Vec<String>) {
        self.labels_added_by_bors.extend(labels.clone());
        for label in labels {
            if !self.labels.contains(&label) {
                self.labels.push(label);
            }
        }
    }

    pub fn remove_label(&mut self, label: &str) {
        self.labels_removed_by_bors.push(label.to_owned());
        self.labels.retain(|l| l != label);
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Branch {
    name: String,
    commits: Vec<Commit>,
    commit_history: Vec<Commit>,
    pub merge_counter: u64,
    pub merge_conflict: bool,
}

impl Branch {
    pub fn new(name: &str, commit: Commit) -> Self {
        Self {
            name: name.to_string(),
            commits: vec![commit.clone()],
            commit_history: vec![commit],
            merge_counter: 0,
            merge_conflict: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn get_commit(&self) -> &Commit {
        self.commits.last().unwrap()
    }
    pub fn get_commits(&self) -> &[Commit] {
        &self.commits
    }
    pub fn sha(&self) -> String {
        self.get_commit().sha().to_owned()
    }

    fn push_commit(&mut self, commit: Commit, force: bool) {
        self.commit_history.push(commit.clone());
        if force {
            self.commits = vec![commit];
        } else {
            self.commits.push(commit);
        }
    }

    pub fn get_commit_history(&self) -> Vec<Commit> {
        self.commit_history.clone()
    }
}

impl Default for Branch {
    fn default() -> Self {
        Self::new(
            default_branch_name(),
            Commit::new(default_branch_sha(), "initial commit"),
        )
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct GitUser {
    pub name: String,
    pub email: String,
}

impl Default for GitUser {
    fn default() -> Self {
        Self {
            name: "git-user".to_string(),
            email: "git-user@git.com".to_string(),
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Commit {
    sha: String,
    message: String,
    author: GitUser,
}

impl Commit {
    pub fn from_sha(sha: &str) -> Self {
        Self::new(sha, &format!("Commit {sha}"))
    }

    pub fn new(sha: &str, message: &str) -> Self {
        Self {
            sha: sha.to_owned(),
            message: message.to_owned(),
            author: GitUser::default(),
        }
    }

    pub fn with_author(mut self, author: GitUser) -> Self {
        self.author = author;
        self
    }

    pub fn sha(&self) -> &str {
        &self.sha
    }
    pub fn commit_sha(&self) -> CommitSha {
        CommitSha(self.sha.to_owned())
    }
    pub fn author(&self) -> &GitUser {
        &self.author
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

#[derive(Clone, Debug)]
pub enum BranchPushError {
    Conflict,
    ValidationFailed,
    InternalServerError,
}

#[derive(Clone, Debug)]
pub enum BranchPushBehaviour {
    Succeed,
    Fail(BranchPushError),
}

impl BranchPushBehaviour {
    pub fn success() -> Self {
        Self::Succeed
    }

    pub fn always_fail(error: BranchPushError) -> Self {
        Self::Fail(error)
    }

    pub fn try_push(&mut self) -> Option<BranchPushError> {
        match self {
            BranchPushBehaviour::Succeed => None,
            BranchPushBehaviour::Fail(error) => Some(error.clone()),
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
    pr_ident: PrIdentifier,
    author: User,
    content: String,
    id: Option<u64>,
    node_id: Option<String>,
    hide_reason: Option<HideCommentReason>,
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

    pub fn pr_ident(&self) -> PrIdentifier {
        self.pr_ident.clone()
    }

    pub fn author(&self) -> User {
        self.author.clone()
    }

    pub fn content(&self) -> String {
        self.content.clone()
    }
    pub fn set_content(&mut self, content: &str) {
        self.content = content.to_owned();
    }

    pub fn id(&self) -> Option<u64> {
        self.id
    }

    pub fn node_id(&self) -> Option<String> {
        self.node_id.clone()
    }

    pub fn hide(&mut self, reason: HideCommentReason) {
        self.hide_reason = Some(reason);
    }
}

impl<'a> From<&'a str> for Comment {
    fn from(value: &'a str) -> Self {
        Comment::new(PrIdentifier::from(()), value)
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
                status: Conclusion::Success,
            },
            run_id,
        }
    }
    pub fn failure(run_id: RunId) -> Self {
        Self {
            event: WorkflowEventKind::Completed {
                status: Conclusion::Failure,
            },
            run_id,
        }
    }
}

#[derive(Clone)]
pub enum WorkflowEventKind {
    Started,
    Completed { status: Conclusion },
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
            head_branch: branch.name().to_string(),
            jobs: vec![],
            head_sha: branch.sha(),
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
