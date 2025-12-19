use crate::bors::{
    CommandPrefix, PullRequestStatus, RollupMode, WAIT_FOR_BUILD_QUEUE, WAIT_FOR_MERGE_QUEUE,
    WAIT_FOR_MERGE_QUEUE_MERGE_ATTEMPT, WAIT_FOR_MERGEABILITY_STATUS_REFRESH, WAIT_FOR_PR_OPEN,
    WAIT_FOR_PR_STATUS_REFRESH, WAIT_FOR_WORKFLOW_COMPLETED, WAIT_FOR_WORKFLOW_STARTED,
};
use crate::database::{
    BuildModel, BuildStatus, DelegatedPermission, MergeableState, OctocrabMergeableState,
    PullRequestModel, WorkflowStatus,
};
use crate::github::PullRequestNumber;
use crate::{
    BorsContext, BorsGlobalEvent, BorsProcess, CommandParser, PgDbClient, TreeState, WebhookSecret,
    create_bors_process, load_repositories,
};
use anyhow::Context;
use axum::Router;
use http::{HeaderMap, Method, Request, StatusCode};
use octocrab::models::RunId;
use octocrab::models::workflows::Conclusion;
use octocrab::params::checks::{CheckRunConclusion, CheckRunStatus};
use parking_lot::Mutex;
use serde::Serialize;
use sqlx::PgPool;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::task::{JoinError, JoinHandle};
use tower::Service;

mod github;
mod mock;
mod utils;

// Public re-exports for use in tests
use crate::bors::process::QueueSenders;
use crate::github::api::client::HideCommentReason;
use crate::server::{ServerState, create_app};
use crate::tests::github::{
    PrIdentifier, RepoIdentifier, TestWorkflowStatus, WorkflowEventKind, WorkflowRun,
    default_oauth_config,
};
use crate::tests::mock::{
    GitHubIssueCommentEventPayload, GitHubPullRequestEventPayload, GitHubPushEventPayload,
    GitHubWorkflowEventPayload, PullRequestChangeEvent,
};
pub use github::Branch;
pub use github::Comment;
pub use github::Commit;
pub use github::GitHub;
pub use github::Permissions;
pub use github::PullRequest;
pub use github::Repo;
pub use github::User;
pub use github::{BranchPushBehaviour, BranchPushError, MergeBehavior};
pub use github::{WorkflowEvent, WorkflowJob};
pub use github::{default_branch_name, default_repo_name};
pub use mock::ExternalHttpMock;
pub use utils::io::load_test_file;
pub use utils::sync::TestSyncMarker;
pub use utils::webhook::{TEST_WEBHOOK_SECRET, create_webhook_request};

/// How long should we wait before we timeout a test.
/// You can increase this if you want to do interactive debugging.
const TEST_TIMEOUT: Duration = Duration::from_secs(15);
/// How long should we wait until a custom condition in a test is hit.
const TEST_CONDITION_TIMEOUT: Duration = Duration::from_secs(10);
/// How often should we check whether a custom condition in a test is hit.
const TEST_CONDITION_CHECK: Duration = Duration::from_millis(100);
/// How long should we wait for a sync marker to be hit.
const SYNC_MARKER_TIMEOUT: Duration = TEST_CONDITION_TIMEOUT;
/// How long should we wait for a comment to be received.
const COMMENT_RECEIVE_TIMEOUT: Duration = TEST_CONDITION_TIMEOUT;

const TRY_BRANCH: &str = "automation/bors/try";
const AUTO_BRANCH: &str = "automation/bors/auto";

pub fn default_cmd_prefix() -> CommandPrefix {
    "@bors".to_string().into()
}

pub struct BorsBuilder {
    github: GitHub,
    pool: PgPool,
}

impl BorsBuilder {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            github: Default::default(),
        }
    }

    pub fn github(self, github: GitHub) -> Self {
        Self { github, ..self }
    }

    /// This closure is used to ensure that the test has to return `BorsTester`
    /// to us, so that we can call `finish()` on it. Without that call, we couldn't
    /// ensure that some async task within the bors process hasn't crashed.
    pub async fn run_test<F: AsyncFnOnce(&mut BorsTester) -> anyhow::Result<()>>(
        self,
        f: F,
    ) -> GitHub {
        // We return `ctx` and `bors` separately, so that we can finish `bors`
        // even if `f` returns an error or times out, for better error propagation.
        let (mut ctx, mut bors) = BorsTester::new(self.pool, self.github).await;

        tokio::select! {
            // If the service ends sooner than the test itself, then the service has panicked.
            // In that case we want to immediately abort the test and print the error.
            res = &mut bors => {
                let error = res.expect_err("Bors service ended unexpectedly without a panic");
                panic!("Bors service has ended unexpectedly: {:?}", join_error_to_anyhow(error));
            }
            result = tokio::time::timeout(TEST_TIMEOUT, f(&mut ctx)) => {
                let gh_state = ctx.finish(bors).await;

                match result {
                    Ok(res) => match res {
                        Ok(_) => gh_state
                            .context("Bors service has failed")
                            // This makes the error nicer
                            .map_err(|e| e.to_string())
                            .unwrap(),
                        Err(error) => {
                            panic!(
                                "Test has failed: {error:?}\n\nBors service error:\n{:?}",
                                gh_state.err()
                            );
                        }
                    },
                    Err(_) => {
                        panic!(
                            "Test has timeouted after {}s\n\nBors service error:\n{:?}",
                            TEST_TIMEOUT.as_secs(),
                            gh_state.err()
                        );
                    }
                }
            }
        }
    }
}

impl From<PgPool> for BorsBuilder {
    fn from(pool: PgPool) -> Self {
        Self {
            pool,
            github: GitHub::default(),
        }
    }
}

impl From<(PgPool, GitHub)> for BorsBuilder {
    fn from((pool, github): (PgPool, GitHub)) -> Self {
        Self { pool, github }
    }
}

/// Simple end-to-end test entrypoint for tests that don't need to prepare any custom state.
/// See [GitHub::default] for how does the default state look like.
pub async fn run_test<
    T: Into<BorsBuilder>,
    F: AsyncFnOnce(&mut BorsTester) -> anyhow::Result<()>,
>(
    args: T,
    f: F,
) -> GitHub {
    let builder: BorsBuilder = args.into();
    builder.run_test(f).await
}

/// Represents a running bors web application. This structure should be used
/// in tests to test interaction with the bot.
///
/// Dropping this struct will drop `app`, which will close the
/// send channels for the bors process, which should stop its async task.
pub struct BorsTester {
    app: Router,
    http_mock: ExternalHttpMock,
    github: Arc<Mutex<GitHub>>,
    db: Arc<PgDbClient>,
    // Sender for bors global events
    global_tx: Sender<BorsGlobalEvent>,
    senders: QueueSenders,
}

impl BorsTester {
    async fn new(pool: PgPool, github: GitHub) -> (Self, JoinHandle<()>) {
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
            senders,
            bors_process,
        } = create_bors_process(
            ctx,
            mock.github_client(),
            mock.team_api_client(),
            chrono::Duration::seconds(1),
        );

        let oauth_config = default_oauth_config();
        let oauth_client = mock.oauth_client(oauth_config);

        let state = ServerState::new(
            repository_tx,
            global_tx.clone(),
            WebhookSecret::new(TEST_WEBHOOK_SECRET.to_string()),
            Some(oauth_client),
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
                senders,
                global_tx,
            },
            bors,
        )
    }

    pub fn db(&self) -> Arc<PgDbClient> {
        self.db.clone()
    }

    /// Return the default repo
    pub fn repo(&self) -> Arc<Mutex<Repo>> {
        self.get_repo(())
    }

    pub fn get_repo<Id: Into<RepoIdentifier>>(&self, id: Id) -> Arc<Mutex<Repo>> {
        self.github.lock().get_repo(id)
    }

    /// Modifies the given repo state in the GitHub mock (**without sending a webhook**).
    pub fn modify_repo<F: FnOnce(&mut Repo) -> R, Id: Into<RepoIdentifier>, R>(
        &mut self,
        id: Id,
        func: F,
    ) -> R {
        let repo = self.github.lock().get_repo(id);
        let mut repo = repo.lock();
        func(&mut repo)
    }

    pub fn try_workflow(&self) -> RunId {
        self.create_workflow(default_repo_name(), TRY_BRANCH)
    }

    pub fn auto_workflow(&self) -> RunId {
        self.create_workflow(default_repo_name(), AUTO_BRANCH)
    }

    pub fn create_workflow<Id: Into<RepoIdentifier>>(&self, id: Id, branch: &str) -> RunId {
        let mut gh = self.github.lock();
        gh.new_workflow(&id.into().0, branch)
    }

    pub fn modify_workflow<F: FnOnce(&mut WorkflowRun)>(&mut self, run_id: RunId, func: F) {
        let repo = self.github.lock().get_repo_by_run_id(run_id);
        let mut repo = repo.lock();
        func(repo.get_workflow_mut(run_id));
    }

    /// Get a PR proxy that can be used to assert various things about the PR.
    pub async fn pr<Id: Into<PrIdentifier>>(&self, id: Id) -> PullRequestProxy {
        let id = id.into();
        let pr = self.get_repo(id.repo).lock().get_pr(id.number).clone();
        PullRequestProxy::new(self, pr).await
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

    pub fn try_branch(&self) -> Branch {
        self.repo()
            .lock()
            .get_branch_by_name(TRY_BRANCH)
            .unwrap()
            .clone()
    }

    pub fn auto_branch(&self) -> Branch {
        self.repo()
            .lock()
            .get_branch_by_name(AUTO_BRANCH)
            .unwrap()
            .clone()
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

        repo.add_branch(Branch::new(
            name,
            Commit::new("initial-sha", &format!("{name}-initial")),
        ));
        repo.get_branch_by_name(name).unwrap().clone()
    }

    /// Modifies a branch on the default repository.
    /// If it doesn't exist yet, it will be created.
    pub fn modify_branch<F: FnOnce(&mut Branch)>(&mut self, name: &str, func: F) {
        let repo = self
            .github
            .lock()
            .repos
            .get(&default_repo_name())
            .unwrap()
            .clone();
        if let Some(branch) = repo.lock().get_branch_by_name(name) {
            func(branch);
        } else {
            self.create_branch(name);
            func(repo.lock().get_branch_by_name(name).unwrap());
        }
    }

    pub async fn push_to_branch(&mut self, branch: &str, commit: Commit) -> anyhow::Result<()> {
        let payload = {
            let gh = self.github.lock();
            let repo = gh.get_repo(default_repo_name());
            let mut repo = repo.lock();
            repo.push_commit(branch, commit.clone());
            GitHubPushEventPayload::new(&repo, branch, commit.sha())
        };
        self.send_webhook("push", payload).await
    }

    /// Wait until the next bot comment is received on the specified repo and PR, and return its
    /// text.
    pub async fn get_next_comment_text<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<String> {
        Ok(GitHub::get_next_comment(self.github.clone(), id)
            .await?
            .content())
    }

    /// Wait until the next bot comment is received on the specified repo and PR, and return it.
    pub async fn get_next_comment<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<Comment> {
        GitHub::get_next_comment(self.github.clone(), id).await
    }

    pub async fn post_comment<C: Into<Comment>>(&mut self, comment: C) -> anyhow::Result<Comment> {
        let comment = comment.into();

        // Allocate comment IDs
        let comment = {
            let gh = self.github.lock();
            let repo = gh.get_repo(&comment.pr_ident().repo);
            let mut repo = repo.lock();
            let pr = repo.get_pr_mut(comment.pr_ident().number);
            let (id, node_id) = pr.next_comment_ids();
            let comment = comment.with_ids(id, node_id);
            pr.add_comment_to_history(comment.clone());
            comment
        };

        self.webhook_comment(comment.clone()).await?;
        Ok(comment)
    }

    pub async fn approve<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<()> {
        let id = id.into();
        self.post_comment(Comment::new(id.clone(), "@bors r+"))
            .await?;
        self.expect_comments(id, 1).await;
        Ok(())
    }

    pub async fn refresh_pending_builds(&self) {
        // Wait until the refresh is fully handled
        wait_for_marker(
            async || {
                self.global_tx
                    .send(BorsGlobalEvent::RefreshPendingBuilds)
                    .await
                    .unwrap();
                Ok(())
            },
            &WAIT_FOR_BUILD_QUEUE,
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

    /// Trigger the merge queue immediately and wait until it performs a single tick.
    /// Ensure that the merge queue is ready for what it is supposed to do in the test when you
    /// call this function. If that is not the case, it might lead to race conditions.
    /// It is more robust to call `run_merge_queue_until_merge_attempt` instead.
    ///
    /// In particular, if you want to ensure that the merge queue has seen a workflow completed
    /// event, you should really run `run_merge_queue_until_merge_attempt`.
    pub async fn run_merge_queue_now(&self) {
        wait_for_marker(
            async || {
                self.senders.merge_queue().perform_tick().await.unwrap();
                Ok(())
            },
            &WAIT_FOR_MERGE_QUEUE,
        )
        .await
        .unwrap();
    }

    /// Repeatedly trigger the merge queue until it attempts to perform a merge.
    /// When that happens, wait until the merge queue completely finishes processing.
    pub async fn run_merge_queue_until_merge_attempt(&self) {
        WAIT_FOR_MERGE_QUEUE_MERGE_ATTEMPT.drain().await;

        let make_merge_queue_fut = || {
            Box::pin(async move {
                self.run_merge_queue_now().await;
                tokio::time::sleep(TEST_CONDITION_CHECK).await;
            })
        };
        let mut wait_fut = std::pin::pin!(WAIT_FOR_MERGE_QUEUE_MERGE_ATTEMPT.sync());
        let mut merge_queue_fut = make_merge_queue_fut();

        loop {
            tokio::select! {
                _ = &mut merge_queue_fut => {
                    merge_queue_fut = make_merge_queue_fut();
                }
                _ = &mut wait_fut => {
                    // Now wait until the merge queue finishes
                    merge_queue_fut.await;
                    break;
                }
            }
        }
    }

    /// Performs a single started/success/failure workflow event.
    pub async fn workflow_event(&mut self, event: WorkflowEvent) -> anyhow::Result<()> {
        // Update the status of the workflow in the GitHub state mock
        let payload = {
            let repo = self.github.lock().get_repo_by_run_id(event.run_id);
            let mut repo = repo.lock();
            let status = match &event.event {
                WorkflowEventKind::Started => WorkflowStatus::Pending,
                WorkflowEventKind::Completed { status } => match status {
                    Conclusion::Success => WorkflowStatus::Success,
                    Conclusion::Failure => WorkflowStatus::Failure,
                    _ => unreachable!(),
                },
            };

            let workflow = repo.get_workflow_mut(event.run_id);
            workflow.change_status(status);
            let workflow = workflow.clone();

            // Box is here to prevent stack overflow in debug mode
            Box::new(GitHubWorkflowEventPayload::new(
                &repo,
                workflow,
                event.event.clone(),
            ))
        };

        let marker = match &event.event {
            WorkflowEventKind::Started => &WAIT_FOR_WORKFLOW_STARTED,
            WorkflowEventKind::Completed { .. } => &WAIT_FOR_WORKFLOW_COMPLETED,
        };

        let fut = self.send_webhook("workflow_run", payload);
        wait_for_marker(async || fut.await, marker).await
    }

    /// Start a workflow and wait until the workflow has been handled by bors.
    pub async fn workflow_start(&mut self, run_id: RunId) -> anyhow::Result<()> {
        self.workflow_event(WorkflowEvent::started(run_id)).await
    }

    pub async fn workflow_full_success(&mut self, run_id: RunId) -> anyhow::Result<()> {
        self.workflow_full(run_id, TestWorkflowStatus::Success)
            .await
    }

    pub async fn workflow_full_failure(&mut self, run_id: RunId) -> anyhow::Result<()> {
        self.workflow_full(run_id, TestWorkflowStatus::Failure)
            .await
    }

    /// Creates a new PR and sends a webhook about its creation.
    /// If you want to set some non-default properties on the PR,
    /// use the `modify_pr` callback, to avoid race conditions with the webhook being sent.
    pub async fn open_pr<F: FnOnce(&mut PullRequest), Id: Into<RepoIdentifier>>(
        &mut self,
        repo: Id,
        modify_pr: F,
    ) -> anyhow::Result<PullRequest> {
        let repo = repo.into();

        // Add the PR to the repository
        let (payload, pr) = {
            let gh = self.github.lock();
            let repo = gh.get_repo(repo);
            let mut repo = repo.lock();
            let pr = repo.add_pr(User::default_pr_author());
            modify_pr(pr);
            let pr = pr.clone();

            let payload = GitHubPullRequestEventPayload::new(&repo, pr.clone(), "opened", None);
            (payload, pr)
        };

        wait_for_marker(
            async || self.send_webhook("pull_request", payload).await,
            &WAIT_FOR_PR_OPEN,
        )
        .await?;
        Ok(pr)
    }

    /// Modifies the given PR state in the GitHub mock (**without sending a webhook**)
    /// and returns a copy of it.
    pub fn modify_pr<Id: Into<PrIdentifier>, F: FnOnce(&mut PullRequest)>(
        &mut self,
        id: Id,
        func: F,
    ) -> PullRequest {
        let id = id.into();
        let repo = self.github.lock().get_repo(&id.repo);
        let mut repo = repo.lock();
        let pr = repo.pulls_mut().get_mut(&id.number).expect("PR must exist");
        func(pr);
        pr.clone()
    }

    pub async fn reopen_pr<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<()> {
        let id = id.into();
        let pr = self.modify_pr(id, |pr| pr.reopen());

        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(&gh.get_repo(&pr.repo).lock(), pr, "reopened", None)
        };

        self.send_webhook("pull_request", payload).await?;
        Ok(())
    }

    pub async fn set_pr_status_closed<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.close());

        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(&gh.get_repo(&pr.repo).lock(), pr, "closed", None)
        };
        self.send_webhook("pull_request", payload).await?;
        Ok(())
    }

    pub async fn set_pr_status_draft<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.convert_to_draft());

        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(
                &gh.get_repo(&pr.repo).lock(),
                pr,
                "converted_to_draft",
                None,
            )
        };
        self.send_webhook("pull_request", payload).await?;
        Ok(())
    }

    pub async fn set_pr_status_ready_for_review<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.ready_for_review());

        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(
                &gh.get_repo(&pr.repo).lock(),
                pr,
                "ready_for_review",
                None,
            )
        };
        self.send_webhook("pull_request", payload).await?;
        Ok(())
    }

    pub async fn set_pr_status_merged<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.merge());

        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(&gh.get_repo(&pr.repo).lock(), pr, "closed", None)
        };
        self.send_webhook("pull_request", payload).await?;
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
                    from_base_sha: Some(base_before.get_commit().sha().to_string()),
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
        let payload = {
            let gh = self.github.lock();
            let repo = gh.get_repo(&id.repo);
            let mut repo = repo.lock();

            let counter = repo.get_next_pr_push_counter();

            let pr = repo
                .pulls_mut()
                .get_mut(&id.number)
                .expect("PR must be initialized before pushing to it");
            pr.head_sha = format!("pr-{}-commit-{counter}", id.number);
            pr.mergeable_state = OctocrabMergeableState::Unknown;
            let pr = pr.clone();
            GitHubPullRequestEventPayload::new(&repo, pr, "synchronize", None)
        };

        self.send_webhook("pull_request", payload).await
    }

    pub async fn assign_pr<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
        assignee: User,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.assignees.push(assignee.clone()));
        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(&gh.get_repo(&pr.repo).lock(), pr, "assigned", None)
        };
        self.send_webhook("pull_request", payload).await
    }

    pub async fn unassign_pr<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
        assignee: User,
    ) -> anyhow::Result<()> {
        let pr = self.modify_pr(id, |pr| pr.assignees.retain(|a| a != &assignee));
        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(
                &gh.get_repo(&pr.repo).lock(),
                pr,
                "unassigned",
                None,
            )
        };
        self.send_webhook("pull_request", payload).await
    }

    /// Starts an auto build, with the expectation that it will start testing the given PR.
    pub async fn start_auto_build<Id: Into<PrIdentifier>>(&mut self, id: Id) -> anyhow::Result<()> {
        let id = id.into();

        let previous_sha = self
            .github
            .lock()
            .get_repo(&id.repo)
            .lock()
            .get_branch_by_name(AUTO_BRANCH)
            .map(|b| b.sha());

        self.run_merge_queue_now().await;
        let comment = self.get_next_comment_text(id).await?;
        assert!(comment.contains("Testing commit"));
        let new_auto_sha = self.auto_branch().sha();
        if let Some(previous_sha) = previous_sha {
            assert_ne!(previous_sha, new_auto_sha, "Auto SHA did not change");
        }

        Ok(())
    }

    /// Perform a successful workflow on the auto branch and run the merge queue, with the
    /// expectation that it will finish the auto build and merge the given PR.
    pub async fn finish_auto_build<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        self.workflow_full_success(self.auto_workflow()).await?;
        self.run_merge_queue_until_merge_attempt().await;
        let comment = self.get_next_comment_text(id).await?;
        assert!(comment.contains("Test successful"));
        Ok(())
    }

    /// Start and then finish auto build on the given PR.
    pub async fn start_and_finish_auto_build<Id: Into<PrIdentifier>>(
        &mut self,
        id: Id,
    ) -> anyhow::Result<()> {
        let id = id.into();
        self.start_auto_build(id.clone()).await?;
        self.finish_auto_build(id).await
    }

    /// Expect that `count` comments will be received, without checking their contents.
    pub async fn expect_comments<Id: Into<PrIdentifier>>(&mut self, id: Id, count: u64) {
        let id = id.into();
        for i in 0..count {
            let id = id.clone();
            self.get_next_comment_text(id)
                .await
                .unwrap_or_else(|e| panic!("Failed to get comment #{i}: {e:?}"));
        }
    }

    /// Assert that (exactly) these workflows have been cancelled.
    pub fn expect_cancelled_workflows<Id: Into<RepoIdentifier>>(&self, id: Id, ids: &[RunId]) {
        self.github
            .lock()
            .check_cancelled_workflows(id.into().0, ids);
    }

    /// Assert that the given comment has been hidden.
    pub fn expect_hidden_comment(&self, comment: &Comment, reason: HideCommentReason) {
        self.github
            .lock()
            .check_comment_was_hidden(comment.node_id().as_deref().unwrap(), reason);
    }

    /// Get a GitHub comment that might have been modified by API calls from bors.
    pub fn get_comment_by_node_id(&self, node_id: &str) -> Option<Comment> {
        self.github.lock().get_comment_by_node_id(node_id).cloned()
    }

    pub fn expect_check_run(
        &self,
        head_sha: &str,
        name: &str,
        title: &str,
        status: CheckRunStatus,
        conclusion: Option<CheckRunConclusion>,
    ) -> &Self {
        let repo = self.repo();
        let repo = repo.lock();
        let check_runs: Vec<_> = repo
            .check_runs()
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

    pub async fn api_request(&mut self, request: ApiRequest) -> anyhow::Result<ApiResponse> {
        let mut path = request.path;
        for (index, (key, value)) in request.query.iter().enumerate() {
            let c = if index == 0 { "?" } else { "&" };
            write!(path, "{c}{key}={}", urlencoding::encode(value))?;
        }

        let response = self
            .app
            .call(
                Request::builder()
                    .uri(&path)
                    .method(request.method)
                    .body(String::new())
                    .unwrap(),
            )
            .await
            .context("Cannot send REST API request")?;
        let headers = response.headers().clone();
        let status = response.status();
        let body_text = String::from_utf8(
            axum::body::to_bytes(response.into_body(), 10 * 1024 * 1024)
                .await?
                .to_vec(),
        );
        Ok(ApiResponse {
            status,
            headers,
            body: body_text?,
        })
    }

    //-- Internal helper functions --/
    /// Wait until the given condition is true.
    /// Checks the condition every 500ms.
    /// Times out if it takes too long (more than 5s).
    ///
    /// This method is useful if you execute a command that produces no comment as an output
    /// and you need to wait until it has been processed by bors.
    /// Prefer using [BorsTester::expect_comments] or [BorsTester::get_next_comment_text] to synchronize
    /// if you are waiting for a comment to be posted to a PR.
    async fn wait_for<F, Fut>(&self, condition: F) -> anyhow::Result<()>
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
                            tokio::time::sleep(TEST_CONDITION_CHECK).await;
                        }
                    }
                    Err(error) => {
                        return Err(error);
                    }
                }
            }
        };
        tokio::time::timeout(TEST_CONDITION_TIMEOUT, wait_fut)
            .await
            .unwrap_or_else(|_| Err(anyhow::anyhow!("Timed out waiting for condition")))
    }

    /// Performs all necessary events to complete a single workflow (start, success/fail).
    async fn workflow_full(
        &mut self,
        run_id: RunId,
        status: TestWorkflowStatus,
    ) -> anyhow::Result<()> {
        self.workflow_event(WorkflowEvent::started(run_id)).await?;
        let event = match status {
            TestWorkflowStatus::Success => WorkflowEvent::success(run_id),
            TestWorkflowStatus::Failure => WorkflowEvent::failure(run_id),
        };
        self.workflow_event(event).await
    }

    async fn webhook_comment(&mut self, comment: Comment) -> anyhow::Result<()> {
        let payload = {
            let gh = self.github.lock();
            GitHubIssueCommentEventPayload::new(
                &gh.get_repo(&comment.pr_ident().repo).lock(),
                comment,
            )
        };

        // The Box is here to prevent a stack overflow in debug mode
        let payload = Box::from(payload);
        self.send_webhook("issue_comment", payload).await
    }

    async fn pull_request_edited(
        &mut self,
        pr: PullRequest,
        changes: Option<PullRequestChangeEvent>,
    ) -> anyhow::Result<()> {
        let payload = {
            let gh = self.github.lock();
            GitHubPullRequestEventPayload::new(
                &gh.get_repo(&pr.repo).lock(),
                pr,
                "edited",
                Some(changes.unwrap_or_default()),
            )
        };
        self.send_webhook("pull_request", payload).await
    }

    async fn send_webhook<S: Serialize>(&mut self, event: &str, content: S) -> anyhow::Result<()> {
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

    async fn try_get_pr_from_db<Id: Into<PrIdentifier>>(
        &self,
        id: Id,
    ) -> anyhow::Result<Option<PullRequestModel>> {
        let id = id.into();
        self.db()
            .get_pull_request(&id.repo, PullRequestNumber(id.number))
            .await
    }

    async fn finish(self, bors: JoinHandle<()>) -> anyhow::Result<GitHub> {
        // Make sure that the event channel senders are closed
        drop(self.app);
        drop(self.global_tx);
        self.senders.mergeability_queue().shutdown();
        drop(self.senders);
        // Wait until all events are handled in the bors service
        match tokio::time::timeout(Duration::from_secs(5), bors).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => return Err(join_error_to_anyhow(error)),
            Err(_) => {
                return Err(anyhow::anyhow!(
                    "Timed out waiting for bors service to shutdown. Maybe you forgot to close some channel senders?"
                ));
            }
        };
        // Flush any local queues
        self.http_mock.gh_server.assert_empty_queues().await?;
        Ok(Arc::into_inner(self.github).unwrap().into_inner())
    }
}

pub struct ApiRequest {
    method: Method,
    path: String,
    query: BTreeMap<String, String>,
}

impl ApiRequest {
    pub fn get(path: &str) -> Self {
        Self {
            method: Method::GET,
            path: path.to_string(),
            query: Default::default(),
        }
    }
    pub fn query(mut self, key: &str, value: &str) -> Self {
        self.query.insert(key.to_string(), value.to_string());
        self
    }
}

pub struct ApiResponse {
    status: StatusCode,
    body: String,
    headers: HeaderMap,
}

impl ApiResponse {
    pub fn assert_ok(self) -> Self {
        if !self.status.is_success() {
            panic!(
                "HTTP response was not success. Status code: {}, body:\n{}",
                self.status, self.body
            );
        }
        self
    }
    pub fn assert_status(self, status: StatusCode) -> Self {
        if self.status != status {
            panic!(
                "HTTP response did not have expected status `{status}`. Status code: {}, body:\n{}",
                self.status, self.body
            );
        }
        self
    }
    pub fn assert_body(&self, body: &str) {
        if self.body != body {
            panic!(
                "Expected HTTP response body `{body}`. Actual body:\n{}",
                self.body
            );
        }
    }
    pub fn get_header(&self, name: &str) -> String {
        self.headers
            .get(name)
            .unwrap_or_else(|| panic!("Header {name} not found"))
            .to_str()
            .unwrap()
            .to_string()
    }
    pub fn into_body(self) -> String {
        self.body
    }
}

/// Try to produce a nicer error message from a failed tokio Task
/// Normally, JoinError internally prints the
/// panic payload with Debug impl instead of Display impl, which mangles backtraces from anyhow
/// errors.
fn join_error_to_anyhow(error: JoinError) -> anyhow::Error {
    let panic = error.into_panic();
    if let Some(s) = panic.downcast_ref::<String>() {
        anyhow::anyhow!("{s}")
    } else if let Some(s) = panic.downcast_ref::<&'static str>() {
        anyhow::anyhow!("{s}")
    } else {
        anyhow::anyhow!("Unknown error")
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
    async fn new(ctx: &BorsTester, gh_pr: PullRequest) -> Self {
        let db_pr = ctx
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
    pub fn expect_auto_build<F>(&self, f: F) -> &Self
    where
        F: FnOnce(&BuildModel) -> bool,
    {
        let auto_build = self
            .require_db_pr()
            .auto_build
            .as_ref()
            .expect("No auto build found");
        assert!(f(auto_build), "Auto build check was not successful");
        self
    }

    #[track_caller]
    pub fn expect_no_auto_build(&self) -> &Self {
        assert!(
            self.require_db_pr().auto_build.is_none(),
            "Auto build should not be on PR {}",
            self.gh_pr.id()
        );
        self
    }

    #[track_caller]
    pub fn expect_mergeable_state(&self, state: MergeableState) -> &Self {
        assert_eq!(self.require_db_pr().mergeable_state, state);
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
        tokio::time::timeout(SYNC_MARKER_TIMEOUT, marker.sync())
            .await
            .map_err(|_| anyhow::anyhow!("Timed out waiting for a test marker to be marked"))?;
    }
    res
}
