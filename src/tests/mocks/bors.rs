use anyhow::Context;
use axum::Router;
use parking_lot::lock_api::MappedMutexGuard;
use parking_lot::{Mutex, MutexGuard, RawMutex};
use serde::Serialize;
use sqlx::PgPool;
use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::task::JoinHandle;
use tower::Service;

use crate::bors::mergeable_queue::MergeableQueueSender;
use crate::bors::{
    RollupMode, WAIT_FOR_CANCEL_TIMED_OUT_BUILDS_REFRESH, WAIT_FOR_MERGEABILITY_STATUS_REFRESH,
    WAIT_FOR_PR_STATUS_REFRESH,
};
use crate::database::{BuildStatus, DelegatedPermission, OctocrabMergeableState, PullRequestModel};
use crate::github::api::load_repositories;
use crate::github::server::BorsProcess;
use crate::github::{GithubRepoName, PullRequestNumber};
use crate::tests::mocks::comment::{Comment, GitHubIssueCommentEventPayload};
use crate::tests::mocks::workflow::{
    CheckSuite, GitHubCheckRunEventPayload, GitHubCheckSuiteEventPayload,
    GitHubWorkflowEventPayload, TestWorkflowStatus, Workflow, WorkflowEvent, WorkflowEventKind,
};
use crate::tests::mocks::{
    Branch, ExternalHttpMock, GitHubState, Repo, User, default_pr_number, default_repo_name,
};
use crate::tests::webhook::{TEST_WEBHOOK_SECRET, create_webhook_request};
use crate::{
    BorsContext, BorsGlobalEvent, CommandParser, PgDbClient, ServerState, WebhookSecret,
    create_app, create_bors_process,
};

use super::pull_request::{
    GitHubPullRequestEventPayload, GitHubPushEventPayload, PullRequestChangeEvent,
};
use super::repository::PullRequest;

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
    pub async fn run_test<
        F: FnOnce(BorsTester) -> Fut,
        Fut: Future<Output = anyhow::Result<BorsTester>>,
    >(
        self,
        f: F,
    ) -> GitHubState {
        // We return `tester` and `bors` separately, so that we can finish `bors`
        // even if `f` returns a result, for better error propagation.
        let (tester, bors) = BorsTester::new(self.pool, self.github).await;
        match f(tester).await {
            Ok(tester) => tester.finish(bors).await,
            Err(error) => {
                let result = bors.await;
                panic!("Error in test:\n{error:?}\n\nBors service result:\n{result:?}");
            }
        }
    }
}

/// Simple end-to-end test entrypoint for tests that don't need to prepare any custom state.
/// See [GitHubState::default] for how does the default state look like.
pub async fn run_test<
    F: FnOnce(BorsTester) -> Fut,
    Fut: Future<Output = anyhow::Result<BorsTester>>,
>(
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
    github: GitHubState,
    db: Arc<PgDbClient>,
    mergeable_queue_tx: MergeableQueueSender,
    // Sender for bors global events
    global_tx: Sender<BorsGlobalEvent>,
    // When this field is false, no webhooks should be generated from BorsTester methods
    webhooks_active: bool,
}

impl BorsTester {
    async fn new(pool: PgPool, github: GitHubState) -> (Self, JoinHandle<()>) {
        let mock = ExternalHttpMock::start(&github).await;
        let db = Arc::new(PgDbClient::new(pool));

        let loaded_repos = load_repositories(&mock.github_client(), &mock.team_api_client())
            .await
            .unwrap();
        let mut repos = HashMap::default();
        for (name, repo) in loaded_repos {
            let repo = repo.unwrap();
            repos.insert(name, Arc::new(repo));
        }

        let ctx = BorsContext::new(CommandParser::new("@bors".to_string()), db.clone(), repos);

        let BorsProcess {
            repository_tx,
            global_tx,
            mergeable_queue_tx,
            bors_process,
        } = create_bors_process(ctx, mock.github_client(), mock.team_api_client());

        let state = ServerState::new(
            repository_tx,
            global_tx.clone(),
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
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
        self.github.get_repo(&default_repo_name())
    }

    pub async fn default_pr(&self) -> PullRequestProxy {
        let pr = self
            .default_repo()
            .lock()
            .get_pr(default_pr_number())
            .clone();
        PullRequestProxy::new(self, pr).await
    }

    pub async fn pr_db(
        &self,
        repo: GithubRepoName,
        number: u64,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        self.db()
            .get_pull_request(&repo, PullRequestNumber(number))
            .await
    }

    pub async fn default_pr_db(&self) -> anyhow::Result<Option<PullRequestModel>> {
        self.pr_db(default_repo_name(), default_pr_number()).await
    }

    /// Wait until a pull request is in the database and satisfies a given condition.
    ///
    /// This is a convenience wrapper around `wait_for` that simplifies checking for PR conditions.
    pub async fn wait_for_pr<F>(
        &self,
        repo: GithubRepoName,
        pr_number: u64,
        condition: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&PullRequestModel) -> bool,
    {
        self.wait_for(|| async {
            let Some(pr) = self.pr_db(repo.clone(), pr_number).await? else {
                return Ok(false);
            };
            Ok(condition(&pr))
        })
        .await
    }

    /// Wait until the default pull request is in the database and satisfies a given condition.
    pub async fn wait_for_default_pr<F>(&self, condition: F) -> anyhow::Result<()>
    where
        F: Fn(&PullRequestModel) -> bool,
    {
        self.wait_for_pr(default_repo_name(), default_pr_number(), condition)
            .await
    }

    pub fn create_branch(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        // We cannot clone the Arc, otherwise it won't work
        let repo = self.github.repos.get(&default_repo_name()).unwrap();
        let mut repo = repo.lock();

        // Polonius where art thou :/
        if repo.get_branch_by_name(name).is_some() {
            MutexGuard::map(repo, |repo| repo.get_branch_by_name(name).unwrap())
        } else {
            MutexGuard::map(repo, move |repo| {
                repo.branches
                    .push(Branch::new(name, &format!("{name}-initial")));
                repo.branches.last_mut().unwrap()
            })
        }
    }

    pub fn get_branch_mut(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        let repo = self.github.repos.get(&default_repo_name()).unwrap();
        MutexGuard::map(repo.lock(), move |repo| {
            repo.get_branch_by_name(name).unwrap()
        })
    }

    pub fn get_branch(&self, name: &str) -> Branch {
        self.github
            .default_repo()
            .lock()
            .get_branch_by_name(name)
            .unwrap()
            .clone()
    }

    pub async fn push_to_branch(&mut self, branch: &str) -> anyhow::Result<()> {
        self.send_webhook("push", GitHubPushEventPayload::new(branch))
            .await
    }

    pub fn try_branch(&self) -> Branch {
        self.get_branch("automation/bors/try")
    }

    /// Wait until the next bot comment is received on the default repo and the default PR.
    pub async fn get_comment(&mut self) -> anyhow::Result<String> {
        Ok(self
            .http_mock
            .gh_server
            .get_comment(Repo::default().name, default_pr_number())
            .await?
            .content)
    }

    //-- Generation of GitHub events --//
    pub async fn post_comment<C: Into<Comment>>(&mut self, comment: C) -> anyhow::Result<()> {
        self.webhook_comment(comment.into()).await
    }

    pub async fn cancel_timed_out_builds(&self) {
        self.global_tx
            .send(BorsGlobalEvent::CancelTimedOutBuilds)
            .await
            .unwrap();
        // Wait until the refresh is fully handled
        WAIT_FOR_CANCEL_TIMED_OUT_BUILDS_REFRESH.sync().await;
    }

    pub async fn update_mergeability_status(&self) {
        self.global_tx
            .send(BorsGlobalEvent::RefreshPullRequestMergeability)
            .await
            .unwrap();
        // Wait until the refresh is fully handled
        WAIT_FOR_MERGEABILITY_STATUS_REFRESH.sync().await;
    }

    pub async fn refresh_prs(&self) {
        self.global_tx
            .send(BorsGlobalEvent::RefreshPullRequestState)
            .await
            .unwrap();
        // Wait until the refresh is fully handled
        WAIT_FOR_PR_STATUS_REFRESH.sync().await;
    }

    /// Performs a single started/success/failure workflow event.
    pub async fn workflow_event(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        if let Some(branch) = self
            .github
            .get_repo(&event.workflow.repository.clone())
            .lock()
            .get_branch_by_name(&event.workflow.head_branch)
        {
            match &event.event {
                WorkflowEventKind::Started => {}
                WorkflowEventKind::Completed { status } => {
                    let status = match status.as_str() {
                        "success" => TestWorkflowStatus::Success,
                        "failure" => TestWorkflowStatus::Failure,
                        _ => unreachable!(),
                    };
                    branch.suite_finished(status);
                }
            }
        }
        self.webhook_workflow(event).await
    }

    /// Performs all necessary events to complete a single workflow (start, success/fail,
    /// check suite completed).
    #[inline]
    pub async fn workflow_full<W: Into<Workflow>>(
        &mut self,
        workflow: W,
        status: TestWorkflowStatus,
    ) -> anyhow::Result<()> {
        let workflow = workflow.into();
        let branch = self.get_branch(&workflow.head_branch);

        if !workflow.external {
            self.workflow_event(WorkflowEvent::started(workflow.clone()))
                .await?;
            let event = match status {
                TestWorkflowStatus::Success => WorkflowEvent::success(workflow.clone()),
                TestWorkflowStatus::Failure => WorkflowEvent::failure(workflow.clone()),
            };
            self.workflow_event(event).await?;
        } else {
            self.webhook_external_workflow(workflow).await?;
        }

        self.check_suite(CheckSuite::completed(branch)).await
    }

    pub async fn workflow_success<W: Into<Workflow>>(&mut self, workflow: W) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Success)
            .await
    }

    pub async fn workflow_failure<W: Into<Workflow>>(&mut self, workflow: W) -> anyhow::Result<()> {
        self.workflow_full(workflow, TestWorkflowStatus::Failure)
            .await
    }

    pub async fn check_suite<C: Into<CheckSuite>>(&mut self, check_suite: C) -> anyhow::Result<()> {
        self.webhook_check_suite(check_suite.into()).await
    }

    pub async fn open_pr(
        &mut self,
        repo_name: GithubRepoName,
        is_draft: bool,
    ) -> anyhow::Result<PullRequest> {
        let number = {
            let repo = self.github.get_repo(&repo_name);
            let repo = repo.lock();
            repo.pull_requests.keys().max().copied().unwrap_or(1)
        };

        let pr = PullRequest::new(repo_name, number, User::default_pr_author(), is_draft);
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "opened", None),
        )
        .await?;
        Ok(pr)
    }

    pub async fn close_pr(
        &mut self,
        repo_name: GithubRepoName,
        pr_number: u64,
    ) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo_name);
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must be opened before closing it");
            pr.close_pr();
            pr.clone()
        };
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "closed", None),
        )
        .await?;
        Ok(())
    }

    pub async fn reopen_pr(
        &mut self,
        repo_name: GithubRepoName,
        pr_number: u64,
    ) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo_name);
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must exist before being reopened");
            pr.reopen_pr();
            pr.clone()
        };
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "reopened", None),
        )
        .await?;
        Ok(())
    }

    pub async fn convert_to_draft(
        &mut self,
        repo_name: GithubRepoName,
        pr_number: u64,
    ) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo_name);
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must exist before being converted to draft");
            pr.convert_to_draft();
            pr.clone()
        };
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "converted_to_draft", None),
        )
        .await?;
        Ok(())
    }

    pub async fn ready_for_review(
        &mut self,
        repo_name: GithubRepoName,
        pr_number: u64,
    ) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo_name);
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must exist before being ready for review");
            pr.ready_for_review();
            pr.clone()
        };
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "ready_for_review", None),
        )
        .await?;
        Ok(())
    }

    pub async fn merge_pr(
        &mut self,
        repo_name: GithubRepoName,
        pr_number: u64,
    ) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo_name);
            let mut repo = repo.lock();
            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must be opened before being merged");
            pr.merge_pr();
            pr.clone()
        };
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr.clone(), "closed", None),
        )
        .await?;
        Ok(())
    }

    /// Perform an arbitrary modification of the given PR, and then send the "edited" PR webhook
    /// to bors.
    pub async fn edit_pr<F>(
        &mut self,
        repo: GithubRepoName,
        pr_number: u64,
        func: F,
    ) -> anyhow::Result<()>
    where
        F: FnOnce(&mut PullRequest),
    {
        let repo = self.github.get_repo(&repo);

        let (pr, changes) = {
            let mut repo = repo.lock();
            let pr = repo.get_pr_mut(pr_number);
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

    pub async fn push_to_pr(&mut self, repo: GithubRepoName, pr_number: u64) -> anyhow::Result<()> {
        let pr = {
            let repo = self.github.get_repo(&repo);
            let mut repo = repo.lock();

            let counter = repo.get_next_pr_push_counter();

            let pr = repo
                .pull_requests
                .get_mut(&pr_number)
                .expect("PR must be initialized before pushing to it");
            pr.head_sha = format!("pr-{pr_number}-commit-{counter}");
            pr.mergeable_state = OctocrabMergeableState::Unknown;
            pr.clone()
        };

        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr, "synchronize", None),
        )
        .await
    }

    //-- Test assertions --//
    /// Expect that `count` comments will be received, without checking their contents.
    pub async fn expect_comments(&mut self, count: u64) {
        for i in 0..count {
            self.get_comment()
                .await
                .unwrap_or_else(|_| panic!("Failed to get comment #{i}"));
        }
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

    async fn webhook_external_workflow(&mut self, workflow: Workflow) -> anyhow::Result<()> {
        self.send_webhook("check_run", GitHubCheckRunEventPayload::from(workflow))
            .await
    }

    async fn webhook_check_suite(&mut self, check_suite: CheckSuite) -> anyhow::Result<()> {
        self.send_webhook(
            "check_suite",
            GitHubCheckSuiteEventPayload::from(check_suite),
        )
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

    async fn finish(self, bors: JoinHandle<()>) -> GitHubState {
        // Make sure that the event channel senders are closed
        drop(self.app);
        drop(self.global_tx);
        self.mergeable_queue_tx.shutdown();
        // Wait until all events are handled in the bors service
        bors.await.unwrap();
        // Flush any local queues
        self.http_mock.gh_server.assert_empty_queues().await;
        self.github
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
            .pr_db(gh_pr.repo.clone(), gh_pr.number.0)
            .await
            .unwrap();
        Self { gh_pr, db_pr }
    }

    pub fn get_gh_pr(&self) -> PullRequest {
        self.gh_pr.clone()
    }

    #[track_caller]
    pub fn expect_rollup(&self, rollup: Option<RollupMode>) -> &Self {
        assert_eq!(self.require_db_pr().rollup, rollup);
        self
    }

    #[track_caller]
    pub fn expect_approved_by(&self, approved_by: &str) -> &Self {
        assert_eq!(self.require_db_pr().approver(), Some(approved_by));
        self.gh_pr.check_added_labels(&["approved"]);
        self
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
    fn require_db_pr(&self) -> &PullRequestModel {
        self.db_pr.as_ref().unwrap()
    }
}
