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

use crate::bors::WAIT_FOR_REFRESH;
use crate::database::PullRequestModel;
use crate::github::api::load_repositories;
use crate::github::PullRequestNumber;
use crate::tests::mocks::comment::{Comment, GitHubIssueCommentEventPayload};
use crate::tests::mocks::workflow::{
    CheckSuite, GitHubCheckRunEventPayload, GitHubCheckSuiteEventPayload,
    GitHubWorkflowEventPayload, TestWorkflowStatus, Workflow, WorkflowEvent, WorkflowEventKind,
};
use crate::tests::mocks::{
    default_pr_number, default_repo_name, Branch, ExternalHttpMock, Repo, World,
};
use crate::tests::webhook::{create_webhook_request, TEST_WEBHOOK_SECRET};
use crate::{
    create_app, create_bors_process, BorsContext, BorsGlobalEvent, CommandParser, PgDbClient,
    ServerState, WebhookSecret,
};

use super::pull_request::{GitHubPullRequestEventPayload, PullRequestChangeEvent};

pub struct BorsBuilder {
    world: World,
    pool: PgPool,
}

impl BorsBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            world: Default::default(),
        }
    }

    pub fn world(self, world: World) -> Self {
        Self { world, ..self }
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
    ) -> World {
        // We return `tester` and `bors` separately, so that we can finish `bors`
        // even if `f` returns a result, for better error propagation.
        let (tester, bors) = BorsTester::new(self.pool, self.world).await;
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
pub async fn run_test<
    F: FnOnce(BorsTester) -> Fut,
    Fut: Future<Output = anyhow::Result<BorsTester>>,
>(
    pool: PgPool,
    f: F,
) -> World {
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
    world: World,
    db: Arc<PgDbClient>,
    // Sender for bors global events
    global_tx: Sender<BorsGlobalEvent>,
}

impl BorsTester {
    async fn new(pool: PgPool, world: World) -> (Self, JoinHandle<()>) {
        let mock = ExternalHttpMock::start(&world).await;
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

        let (repository_tx, global_tx, bors_process) =
            create_bors_process(ctx, mock.github_client(), mock.team_api_client());

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
                world,
                db,
                global_tx,
            },
            bors,
        )
    }

    pub fn db(&self) -> Arc<PgDbClient> {
        self.db.clone()
    }

    pub fn default_repo(&self) -> Arc<Mutex<Repo>> {
        self.world.get_repo(default_repo_name())
    }

    pub async fn get_default_pr(&self) -> anyhow::Result<PullRequestModel> {
        self.db()
            .get_or_create_pull_request(
                &default_repo_name(),
                PullRequestNumber(default_pr_number()),
            )
            .await
    }

    pub async fn refresh(&self) {
        self.global_tx.send(BorsGlobalEvent::Refresh).await.unwrap();
        // Wait until the refresh is fully handled
        WAIT_FOR_REFRESH.sync().await;
    }

    pub fn create_branch(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        // We cannot clone the Arc, otherwise it won't work
        let repo = self.world.repos.get(&default_repo_name()).unwrap();
        let mut repo = repo.lock();

        // Polonius where art thou :/
        if repo.get_branch_by_name(name).is_some() {
            MutexGuard::map(repo, |repo| repo.get_branch_by_name(name).unwrap())
        } else {
            MutexGuard::map(repo, move |repo| {
                repo.branches
                    .push(Branch::new(name, &format!("{name}-empty")));
                repo.branches.last_mut().unwrap()
            })
        }
    }

    pub fn get_branch_mut(&mut self, name: &str) -> MappedMutexGuard<RawMutex, Branch> {
        let repo = self.world.repos.get(&default_repo_name()).unwrap();
        MutexGuard::map(repo.lock(), move |repo| {
            repo.get_branch_by_name(name).unwrap()
        })
    }

    pub fn get_branch(&self, name: &str) -> Branch {
        self.world
            .default_repo()
            .lock()
            .get_branch_by_name(name)
            .unwrap()
            .clone()
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

    /// Expect that `count` comments will be received, without checking their contents.
    pub async fn expect_comments(&mut self, count: u64) {
        for i in 0..count {
            self.get_comment()
                .await
                .unwrap_or_else(|_| panic!("Failed to get comment #{i}"));
        }
    }

    pub async fn post_comment<C: Into<Comment>>(&mut self, comment: C) -> anyhow::Result<()> {
        self.webhook_comment(comment.into()).await
    }

    /// Performs a single started/success/failure workflow event.
    pub async fn workflow_event(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        if let Some(branch) = self
            .world
            .get_repo(event.workflow.repository.clone())
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

    pub async fn edit_pull_request(
        &mut self,
        pr_number: u64,
        changes: PullRequestChangeEvent,
    ) -> anyhow::Result<()> {
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr_number, "edited".to_string(), Some(changes)),
        )
        .await
    }

    pub async fn push_to_pull_request(&mut self, pr_number: u64) -> anyhow::Result<()> {
        let repo = self.default_repo();
        let mut repo = repo.lock();

        let count = repo.get_next_pr_push_count();

        let pr = repo
            .pull_requests
            .get_mut(&pr_number)
            .expect("PR must be initialized before pushing to it");
        pr.head_sha = format!("pr-{}-sha-{}", pr_number, count);

        drop(repo);
        self.send_webhook(
            "pull_request",
            GitHubPullRequestEventPayload::new(pr_number, "synchronize".to_string(), None),
        )
        .await
    }

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

    async fn send_webhook<S: Serialize>(&mut self, event: &str, content: S) -> anyhow::Result<()> {
        let serialized = serde_json::to_string(&content).unwrap();
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

    pub async fn finish(self, bors: JoinHandle<()>) -> World {
        // Make sure that the event channel senders are closed
        drop(self.app);
        drop(self.global_tx);
        // Wait until all events are handled in the bors service
        bors.await.unwrap();
        // Flush any local queues
        self.http_mock.gh_server.assert_empty_queues().await;
        self.world
    }
}
