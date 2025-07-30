mod io;
mod mocks;
mod util;
mod webhook;

use anyhow::Context;
use axum::Router;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use parking_lot::Mutex;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tower::Service;

use crate::bors::merge_queue::MergeQueueSender;
use crate::bors::mergeable_queue::MergeableQueueSender;
use crate::bors::{
    CommandPrefix, PullRequestStatus, RollupMode, WAIT_FOR_MERGE_QUEUE,
    WAIT_FOR_MERGEABILITY_STATUS_REFRESH, WAIT_FOR_PR_STATUS_REFRESH,
    WAIT_FOR_REFRESH_PENDING_BUILDS, WAIT_FOR_WORKFLOW_STARTED,
};
use crate::database::{
    BuildStatus, DelegatedPermission, OctocrabMergeableState, PullRequestModel, WorkflowStatus,
};
use crate::github::{GithubRepoName, PullRequestNumber};
use crate::{
    BorsContext, BorsGlobalEvent, BorsProcess, CommandParser, PgDbClient, ServerState, TreeState,
    WebhookSecret, create_app, create_bors_process, load_repositories,
};

use crate::tests::mocks::comment::GitHubIssueCommentEventPayload;
use crate::tests::mocks::pull_request::{
    GitHubPullRequestEventPayload, GitHubPushEventPayload, PrIdentifier, PullRequest,
    PullRequestChangeEvent,
};
use crate::tests::mocks::workflow::{
    GitHubWorkflowEventPayload, TestWorkflowStatus, WorkflowEventKind,
};

// Public re-exports for use in tests
pub use io::load_test_file;
pub use mocks::ExternalHttpMock;
pub use mocks::GitHubState;
pub use mocks::comment::Comment;
pub use mocks::permissions::Permissions;
pub use mocks::repository::{Branch, Repo, default_branch_name, default_repo_name};
pub use mocks::user::User;
pub use mocks::workflow::{WorkflowEvent, WorkflowJob, WorkflowRunData};
pub use util::TestSyncMarker;
pub use webhook::{TEST_WEBHOOK_SECRET, create_webhook_request};

/// How long should we wait before we timeout a test.
/// You can increase this if you want to do interactive debugging.
const TEST_TIMEOUT: Duration = Duration::from_secs(100);

pub fn default_cmd_prefix() -> CommandPrefix {
    "@bors".to_string().into()
}

pub struct BorsBuilder {
    github: GitHubState,
    pool: PgPool,
}

impl BorsBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            github: Default::default(),
        }
    }

    pub fn github(self, github: GitHubState) -> Self {
        Self { github, ..self }
    }

    /// This closure is used to ensure that the test has to return `BorsTester`
    /// to us, so that we can call `finish()` on it. Without that call, we couldn't
    /// ensure that some async task within the bors process hasn't crashed.
    pub async fn run_test<F: AsyncFnOnce(&mut BorsTester) -> anyhow::Result<()>>(
        self,
        f: F,
    ) -> GitHubState {
        // We return `tester` and `bors` separately, so that we can finish `bors`
        // even if `f` returns an error or times out, for better error propagation.
        let (mut tester, bors) = BorsTester::new(self.pool, self.github).await;

        let result = tokio::time::timeout(TEST_TIMEOUT, f(&mut tester)).await;
        let gh_state = tester.finish(bors).await;

        match result {
            Ok(res) => match res {
                Ok(_) => gh_state
                    .context("Bors service has failed")
                    // This makes the error nicer
                    .map_err(|e| e.to_string())
                    .unwrap(),
                Err(error) => {
                    panic!(
                        "Test has failed: {error:?}\n\nBors service error: {:?}",
                        gh_state.err()
                    );
                }
            },
            Err(_) => {
                panic!(
                    "Test has timeouted after {}s\n\nBors service error: {:?}",
                    TEST_TIMEOUT.as_secs(),
                    gh_state.err()
                );
            }
        }
    }
}

/// Simple end-to-end test entrypoint for tests that don't need to prepare any custom state.
/// See [GitHubState::default] for how does the default state look like.
pub async fn run_test<F: AsyncFnOnce(&mut BorsTester) -> anyhow::Result<()>>(
    pool: PgPool,
    f: F,
) -> GitHubState {
    BorsBuilder::new(pool).run_test(f).await
}

/// Represents a running bors web application. This structure should be used
/// in tests to test interaction with the bot.
///
/// Dropping this struct will drop `app`, which will close the
/// send channels for the bors process, which should stop its async task.
pub struct BorsTester {
    app: Router,
    http_mock: ExternalHttpMock,
    github: Arc<Mutex<GitHubState>>,
    db: Arc<PgDbClient>,
    mergeable_queue_tx: MergeableQueueSender,
    merge_queue_tx: MergeQueueSender,
    // Sender for bors global events
    global_tx: Sender<BorsGlobalEvent>,
    // When this field is false, no webhooks should be generated from BorsTester methods
    webhooks_active: bool,
}

impl BorsTester {
    async fn new(pool: PgPool, github: GitHubState) -> (Self, JoinHandle<()>) {
        let github = Arc::new(Mutex::new(github));
        let mock = ExternalHttpMock::start(github.clone()).await;
        let db = Arc::new(PgDbClient::new(pool));

        let loaded_repos = load_repositories(&mock.github_client(), &mock.team_api_client())
            .await
            .unwrap();
        let mut repos = HashMap::default();
        for (name, repo) in loaded_repos {
            let repo = repo.unwrap();
            repos.insert(name.clone(), Arc::new(repo));
        }

        for name in repos.keys() {
            if let Err(error) = db.insert_repo_if_not_exists(name, TreeState::Open).await {
                tracing::warn!("Failed to insert repository {name} in test: {error:?}");
            }
        }

        let ctx = BorsContext::new(
            CommandParser::new("@bors".to_string().into()),
            db.clone(),
            repos.clone(),
            "https://test.com/bors",
        );

        let BorsProcess {
            repository_tx,
            global_tx,
            mergeable_queue_tx,
            merge_queue_tx,
            bors_process,
        } = create_bors_process(ctx, mock.github_client(), mock.team_api_client());

        let state = ServerState::new(
            repository_tx,
            global_tx.clone(),
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
            repos.clone(),
            db.clone(),
            default_cmd_prefix(),
        );
        let app = create_app(state);
        let bors = tokio::spawn(bors_process);
        (
            Self {
                app,
                http_mock: mock,
                github,
                db,
                mergeable_queue_tx,
                merge_queue_tx,
                global_tx,
                webhooks_active: true,
            },
            bors,
        )
    }

    //--- Getters for various test state ---//
    pub fn db(&self) -> Arc<PgDbClient> {
        self.db.clone()
    }

    pub fn default_repo(&self) -> Arc<Mutex<Repo>> {
        self.get_repo(&default_repo_name())
    }

    pub fn get_repo(&self, name: &GithubRepoName) -> Arc<Mutex<Repo>> {
        self.github.lock().get_repo(name)
    }

    /// Get a PR proxy that can be used to assert various things about the PR.
    pub async fn get_pr<Id: Into<PrIdentifier>>(&self, id: Id) -> PullRequestProxy {
        let id = id.into();
        let pr = self.get_repo(&id.repo).lock().get_pr(id.number).clone();
        PullRequestProxy::new(self, pr).await
    }

    async fn try_get_pr_from_db<Id: Into<PrIdentifier>>(
        &self,
        id: Id,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        let id = id.into();
        self.db()
            .get_pull_request(&id.repo, PullRequestNumber(id.number))
            .await
    }

    /// Wait until a pull request is in the database and satisfies a given condition.
    ///
    /// This is a convenience wrapper around `wait_for` that simplifies checking for PR conditions.
    pub async fn wait_for_pr<F, Id: Into<PrIdentifier>>(
        &self,
        id: Id,
        condition: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&PullRequestModel) -> bool,
    {
        let id = id.into();
        self.wait_for(|| {
            let id = id.clone();
            async {
                let Some(pr) = self.try_get_pr_from_db(id).await? else {
                    return Ok(false);
                };
                Ok(condition(&pr))
            }
        })
        .await
    }

    /// Creates a branch and returns a **copy** of it.
    pub fn create_branch(&mut self, name: &str) -> Branch {
        let repo = self
            .github
            .lock()
            .repos
            .get(&default_repo_name())
            .unwrap()
            .clone();
        let mut repo = repo.lock();

        assert!(
            repo.get_branch_by_name(name).is_none(),
            "Branch {name} already exists"
        );
        repo.branches
            .push(Branch::new(name, &format!("{name}-initial")));
        repo.branches.last_mut().unwrap().clone()
    }

    /// Modifies an existing branch on the default repository.
    pub fn modify_branch<F: FnOnce(&mut Branch)>(&mut self, name: &str, func: F) {
        let repo = self
            .github
            .lock()
            .repos
            .get(&default_repo_name())
            .unwrap()
            .clone();
        let mut repo = repo.lock();

        let branch = repo
            .get_branch_by_name(name)
            .expect("Branch does not exist");
        func(branch);
    }

    pub fn get_branch(&self, name: &str) -> Branch {
        self.github
            .lock()
            .default_repo()
            .lock()
            .get_branch_by_name(name)
            .unwrap()
            .clone()
    }

    pub fn get_branch_commit_message(&self, branch: &Branch) -> String {
        self.github
            .lock()
            .default_repo()
            .lock()
            .get_commit_message(branch.get_sha())
    }

    pub async fn push_to_branch(&mut self, branch: &str) -> anyhow::Result<()> {
        self.send_webhook("push", GitHubPushEventPayload::new(branch))
            .await
    }

    pub fn try_branch(&self) -> Branch {
        self.get_branch("automation/bors/try")
    }

    pub fn auto_branch(&self) -> Branch {
        self.get_branch("automation/bors/auto")
    }

    /// Wait until the next bot comment is received on the specified repo and PR.
    pub async fn get_comment<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<String> {
        Ok(self.http_mock.gh_server.get_comment(id).await?.content)
    }

    //-- Generation of GitHub events --//
    pub async fn post_comment<C: Into<Comment>>(&mut self, comment: C) -> anyhow::Result<()> {
        self.webhook_comment(comment.into()).await
    }

    pub async fn cancel_timed_out_builds(&self) {
        // Wait until the refresh is fully handled
        wait_for_marker(
            async || {
                self.global_tx
                    .send(BorsGlobalEvent::RefreshPendingBuilds)
                    .await
                    .unwrap();
                Ok(())
            },
            &WAIT_FOR_REFRESH_PENDING_BUILDS,
        )
        .await
        .unwrap();
    }

    pub async fn update_mergeability_status(&self) {
        // Wait until the refresh is fully handled
        wait_for_marker(
            async || {
                self.global_tx
                    .send(BorsGlobalEvent::RefreshPullRequestMergeability)
                    .await
                    .unwrap();
                Ok(())
            },
            &WAIT_FOR_MERGEABILITY_STATUS_REFRESH,
        )
        .await
        .unwrap();
    }

    pub async fn refresh_prs(&self) {
        // Wait until the refresh is fully handled
        wait_for_marker(
            async || {
                self.global_tx
                    .send(BorsGlobalEvent::RefreshPullRequestState)
                    .await
                    .unwrap();
                Ok(())
            },
            &WAIT_FOR_PR_STATUS_REFRESH,
        )
        .await
        .unwrap();
    }

    pub async fn process_merge_queue(&self) {
        // Wait until the merge queue processing is fully handled
        wait_for_marker(
            async || {
                self.global_tx
                    .send(BorsGlobalEvent::ProcessMergeQueue)
                    .await
                    .unwrap();
                Ok(())
            },
            &WAIT_FOR_MERGE_QUEUE,
        )
        .await
        .unwrap();
    }

    /// Performs a single started/success/failure workflow event.
    pub async fn workflow_event(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        // Update the status of the workflow in the GitHub state mock
        {
            let repo = self
                .github
                .lock()
                .get_repo(&event.workflow.repository.clone());
            let mut repo = repo.lock();
            let status = match &event.event {
                WorkflowEventKind::Started => WorkflowStatus::Pending,
                WorkflowEventKind::Completed { status } => match status.as_str() {
                    "success" => WorkflowStatus::Success,
                    "failure" => WorkflowStatus::Failure,
                    _ => unreachable!(),
                },
            };
            repo.update_workflow_run(event.workflow.clone(), status);
        }
        self.webhook_workflow(event).await
    }

    /// Start a workflow and wait until the workflow has been handled by bors.
    pub async fn workflow_start<W: Into<WorkflowRunData>>(
        &mut self,
        workflow: W,
    ) -> anyhow::Result<()> {
        wait_for_marker(
            async || self.workflow_event(WorkflowEvent::started(workflow)).await,
            &WAIT_FOR_WORKFLOW_STARTED,
        )
        .await
    }

    /// Performs all necessary events to complete a single workflow (start, success/fail).
    #[inline]
    pub async fn workflow_full<W: Into<WorkflowRunData>>(
        &mut self,
        workflow: W,
        status: TestWorkflowStatus,
    ) -> anyhow::Result<()> {
        let workflow = workflow.into();

        self.workflow_event(WorkflowEvent::started(workflow.clone()))
            .await?;
        let event = match status {
            TestWorkflowStatus::Success => WorkflowEvent::success(workflow.clone()),
            TestWorkflowStatus::Failure => WorkflowEvent::failure(workflow.clone()),
        };
        self.workflow_event(event).await
    }

    pub async fn workflow_full_success<W: Into<WorkflowRunData>>(
        &mut self,
        workflow: W,
    ) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Success)
            .await
    }

    pub async fn workflow_full_failure<W: Into<WorkflowRunData>>(
        &mut self,
        workflow: W,
    ) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Failure)
            .await
    }

    /// Creates a new PR and sends a webhook about its creation.
    /// If you want to set some non-default properties on the PR,
    /// use the `modify_pr` callback, to avoid race conditions with the webhook being sent.
    pub async fn open_pr<F: FnOnce(&mut PullRequest)>(
        &mut self,
        repo_name: GithubRepoName,
        modify_pr: F,
    ) -> anyhow::Result<PullRequest> {
        let number = {
            let repo = self.github.lock().get_repo(&repo_name);
            let repo = repo.lock();
            repo.pull_requests.keys().max().copied().unwrap_or(0) + 1
        };

        let mut pr = PullRequest::new(repo_name.clone(), number, User::default_pr_author());
        modify_pr(&mut pr);

        // Add the PR to the repository
        {
            let repo = self.github.lock().get_repo(&repo_name);
            let mut repo = repo.lock();
            repo.pull_requests.insert(number, pr.clone());
        }

        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "opened", None),
        )
        .await?;
        Ok(pr)
    }

    /// Modifies the given PR state in the GitHub mock (**without sending a webhook**)
    /// and returns a copy of it.
    pub fn modify_pr_state<Id: Into<PrIdentifier>, F: FnOnce(&mut PullRequest)>(
        &mut self,
        id: Id,
        func: F,
    ) -> PullRequest {
        let id = id.into();
        let repo = self.github.lock().get_repo(&id.repo);
        let mut repo = repo.lock();
        let pr = repo
            .pull_requests
            .get_mut(&id.number)
            .expect("PR must exist");
        func(pr);
        pr.clone()
    }

    pub async fn reopen_pr<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<()> {
        let id = id.into();
        let pr = self.modify_pr_state(id, |pr| pr.reopen_pr());
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "reopened", None),
        )
        .await?;
        Ok(())
    }

    pub async fn set_pr_status_closed<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.close_pr());
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "closed", None),
        )
        .await?;
        Ok(())
    }

    pub async fn set_pr_status_draft<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.convert_to_draft());
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "converted_to_draft", None),
        )
        .await?;
        Ok(())
    }

    pub async fn set_pr_status_ready_for_review<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.ready_for_review());
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "ready_for_review", None),
        )
        .await?;
        Ok(())
    }

    pub async fn set_pr_status_merged<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.merge_pr());
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "closed", None),
        )
        .await?;
        Ok(())
    }

    /// Perform an arbitrary modification of the given PR, and then send the "edited" PR webhook
    /// to bors.
    pub async fn edit_pr<Id: Into<PrIdentifier>, F>(
        &mut self,
        id: Id,
        func: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&mut PullRequest),
    {
        let id = id.into();
        let repo = self.github.lock().get_repo(&id.repo);

        let (pr, changes) = {
            let mut repo = repo.lock();
            let pr = repo.get_pr_mut(id.number);
            let base_before = pr.base_branch.clone();
            func(pr);

            let changes = if base_before != pr.base_branch {
                Some(PullRequestChangeEvent {
                    from_base_sha: Some(base_before.get_sha().to_string()),
                })
            } else {
                None
            };
            (pr.clone(), changes)
        };

        self.pull_request_edited(pr, changes).await
    }

    pub async fn push_to_pr<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<()> {
        let id = id.into();
        let pr = {
            let repo = self.github.lock().get_repo(&id.repo);
            let mut repo = repo.lock();

            let counter = repo.get_next_pr_push_counter();

            let pr = repo
                .pull_requests
                .get_mut(&id.number)
                .expect("PR must be initialized before pushing to it");
            pr.head_sha = format!("pr-{}-commit-{counter}", id.number);
            pr.mergeable_state = OctocrabMergeableState::Unknown;
            pr.clone()
        };

        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "synchronize", None),
        )
        .await
    }

    pub async fn assign_pr<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
        assignee: User,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.assignees.push(assignee.clone()));
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "assigned", None),
        )
        .await
    }

    pub async fn unassign_pr<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
        assignee: User,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr_state(id, |pr| pr.assignees.retain(|a| a != &assignee));
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "unassigned", None),
        )
        .await
    }

    //-- Test assertions --//
    /// Expect that `count` comments will be received, without checking their contents.
    pub async fn expect_comments<Id: Into<PrIdentifier>>(&mut self, id: Id, count: u64) {
        let id = id.into();
        for i in 0..count {
            let id = id.clone();
            self.get_comment(id)
                .await
                .unwrap_or_else(|_| panic!("Failed to get comment #{i}"));
        }
    }

    #[track_caller]
    pub fn expect_check_run(
        &self,
        head_sha: &str,
        name: &str,
        title: &str,
        status: CheckRunStatus,
        conclusion: Option<CheckRunConclusion>,
    ) -> &Self {
        let repo = self.default_repo();
        let repo = repo.lock();
        let check_runs: Vec<_> = repo
            .check_runs
            .iter()
            .filter(|check_run| check_run.head_sha == head_sha)
            .collect();

        assert_eq!(check_runs.len(), 1);

        let check_run = check_runs[0];
        let expected_status = match status {
            CheckRunStatus::Queued => "queued",
            CheckRunStatus::InProgress => "in_progress",
            CheckRunStatus::Completed => "completed",
        };
        let expected_conclusion = conclusion.map(|c| match c {
            CheckRunConclusion::Success => "success",
            CheckRunConclusion::Failure => "failure",
            CheckRunConclusion::Neutral => "neutral",
            CheckRunConclusion::Cancelled => "cancelled",
            CheckRunConclusion::TimedOut => "timed_out",
            CheckRunConclusion::ActionRequired => "action_required",
            CheckRunConclusion::Stale => "stale",
            CheckRunConclusion::Skipped => "skipped",
        });

        assert_eq!(
            (
                check_run.name.as_str(),
                check_run.head_sha.as_str(),
                check_run.status.as_str(),
                check_run.title.as_str(),
                check_run.conclusion.as_deref(),
                check_run.external_id.parse::<u64>().is_ok()
            ),
            (
                name,
                head_sha,
                expected_status,
                title,
                expected_conclusion,
                true
            )
        );

        self
    }

    /// Wait until the given condition is true.
    /// Checks the condition every 500ms.
    /// Times out if it takes too long (more than 5s).
    ///
    /// This method is useful if you execute a command that produces no comment as an output
    /// and you need to wait until it has been processed by bors.
    /// Prefer using [BorsTester::expect_comments] or [BorsTester::get_comment] to synchronize
    /// if you are waiting for a comment to be posted to a PR.
    pub async fn wait_for<F, Fut>(&self, condition: F) -> anyhow::Result<()>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = anyhow::Result<bool>>,
    {
        let wait_fut = async move {
            loop {
                let fut = condition();
                match fut.await {
                    Ok(res) => {
                        if res {
                            return Ok(());
                        } else {
                            tokio::time::sleep(Duration::from_millis(500)).await;
                        }
                    }
                    Err(error) => {
                        return Err(error);
                    }
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait_fut)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Timed out waiting for condition")))
    }

    /// Temporarily block sent webhooks, to emulate situation where webhooks could be lost,
    /// while `func` is executing.
    pub async fn with_blocked_webhooks<T, F>(&mut self, func: F) -> T
    where
        F: AsyncFnOnce(&mut BorsTester) -> T,
    {
        let orig_webhooks = self.webhooks_active;
        self.webhooks_active = false;
        let result = func(self).await;
        self.webhooks_active = orig_webhooks;
        result
    }

    //-- Internal helper functions --/
    async fn webhook_comment(&mut self, comment: Comment) -> anyhow::Result<()> {
        self.send_webhook(
            "issue_comment",
            // The Box is here to prevent a stack overflow in debug mode
            Box::from(GitHubIssueCommentEventPayload::from(comment)),
        )
        .await
    }

    async fn webhook_workflow(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        self.send_webhook("workflow_run", GitHubWorkflowEventPayload::from(event))
            .await
    }

    async fn pull_request_edited(
        &mut self,
        pr: PullRequest,
        changes: Option<PullRequestChangeEvent>,
    ) -> anyhow::Result<()> {
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "edited", Some(changes.unwrap_or_default())),
        )
        .await
    }

    async fn send_webhook<S: Serialize>(&mut self, event: &str, content: S) -> anyhow::Result<()> {
        if !self.webhooks_active {
            return Ok(());
        }

        let serialized = serde_json::to_string(&content)?;
        let webhook = create_webhook_request(event, &serialized);
        let response = self
            .app
            .call(webhook)
            .await
            .context("Cannot send webhook request")?;
        let status = response.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Wrong status code {status} when sending {event}"
            ));
        }
        let body_text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
                .await?
                .to_vec(),
        )?;
        tracing::debug!("Received webhook with status {status} and response body `{body_text}`");
        Ok(())
    }

    async fn finish(self, bors: JoinHandle<()>) -> anyhow::Result<GitHubState> {
        // Make sure that the event channel senders are closed
        drop(self.app);
        drop(self.global_tx);
        self.merge_queue_tx.shutdown();
        self.mergeable_queue_tx.shutdown();
        // Wait until all events are handled in the bors service
        match tokio::time::timeout(Duration::from_secs(5), bors).await {
            Ok(res) => res?,
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Timed out waiting for bors service to shutdown. Maybe you forgot to close some channel senders?"
                ));
            }
        };
        // Flush any local queues
        self.http_mock.gh_server.assert_empty_queues().await;
        Ok(Arc::into_inner(self.github).unwrap().into_inner())
    }
}

/// A proxy object for checking assertions on a pull request.
/// It creates a state snapshot when it is created, therefore it will not be updated as state
/// on GitHub/database changes.
pub struct PullRequestProxy {
    gh_pr: PullRequest,
    db_pr: Option<PullRequestModel>,
}

impl PullRequestProxy {
    async fn new(tester: &BorsTester, gh_pr: PullRequest) -> Self {
        let db_pr = tester
            .try_get_pr_from_db((gh_pr.repo.clone(), gh_pr.number.0))
            .await
            .unwrap();
        Self { gh_pr, db_pr }
    }

    pub fn get_gh_pr(&self) -> PullRequest {
        self.gh_pr.clone()
    }

    #[track_caller]
    pub fn expect_status(&self, status: PullRequestStatus) -> &Self {
        assert_eq!(self.require_db_pr().pr_status, status);
        self
    }

    #[track_caller]
    pub fn expect_rollup(&self, rollup: Option<RollupMode>) -> &Self {
        assert_eq!(self.require_db_pr().rollup, rollup);
        self
    }

    #[track_caller]
    pub fn expect_approved_by(&self, approved_by: &str) -> &Self {
        assert_eq!(self.require_db_pr().approver(), Some(approved_by));
        self.expect_added_labels(&["approved"])
    }

    #[track_caller]
    pub fn expect_unapproved(&self) -> &Self {
        assert!(!self.require_db_pr().is_approved());
        self
    }

    #[track_caller]
    pub fn expect_priority(&self, priority: Option<i32>) -> &Self {
        assert_eq!(self.require_db_pr().priority, priority);
        self
    }

    #[track_caller]
    pub fn expect_delegated(&self, delegation_type: DelegatedPermission) -> &Self {
        assert_eq!(
            self.require_db_pr().delegated_permission.as_ref().unwrap(),
            &delegation_type
        );
        self
    }

    #[track_caller]
    pub fn expect_approved_sha(&self, sha: &str) -> &Self {
        assert_eq!(self.require_db_pr().approved_sha(), Some(sha));
        self
    }

    #[track_caller]
    pub fn expect_try_build_cancelled(&self) {
        assert_eq!(
            self.require_db_pr().try_build.as_ref().unwrap().status,
            BuildStatus::Cancelled
        );
    }

    #[track_caller]
    pub fn expect_added_labels(&self, labels: &[&str]) -> &Self {
        let added_labels = self
            .gh_pr
            .labels_added_by_bors
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>();
        assert_eq!(&added_labels, labels);
        self
    }

    #[track_caller]
    pub fn expect_removed_labels(&self, labels: &[&str]) -> &Self {
        let removed_labels = self
            .gh_pr
            .labels_removed_by_bors
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>();
        assert_eq!(&removed_labels, labels);
        self
    }

    #[track_caller]
    fn require_db_pr(&self) -> &PullRequestModel {
        self.db_pr.as_ref().unwrap()
    }
}

/// Start an async operation and wait until a specific [`TestSyncMarker`]
/// is marked.
async fn wait_for_marker<Func>(func: Func, marker: &TestSyncMarker) -> anyhow::Result<()>
where
    Func: AsyncFnOnce() -> anyhow::Result<()>,
{
    // Since the `TestSyncMarker` contains a thread-local variable, it could contain some previously
    // marked notifications from previously executed tests that ran on the same thread.
    // Drain those BEFORE starting the current asynchronous operation, so that we can make sure that
    // there are no leftovers.
    marker.drain().await;

    let res = func().await;
    if res.is_ok() {
        tokio::time::timeout(Duration::from_secs(5), marker.sync())
            .await
            .map_err(|_| anyhow::anyhow!("Timed out waiting for a test marker to be marked"))?;
    }
    res
}
