use derive_builder::Builder;

use crate::bors::event::PullRequestComment;
use crate::bors::{event, CheckSuite, CheckSuiteStatus};
use crate::database::WorkflowStatus;
use crate::github::{CommitSha, GithubRepoName, GithubUser};
use crate::tests::state::default_repo_name;

fn default_user() -> GithubUser {
    GithubUser {
        username: "<user>".to_string(),
        html_url: "https://user.com".parse().unwrap(),
    }
}

pub fn default_pr_number() -> u64 {
    1
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Comment {
    #[builder(default = "default_repo_name()")]
    repo: GithubRepoName,
    #[builder(default = "1")]
    pr_number: u64,
    text: String,
    #[builder(default = "default_user()")]
    author: GithubUser,
}

impl CommentBuilder {
    pub fn create(self) -> PullRequestComment {
        let Comment {
            repo,
            pr_number,
            text,
            author,
        } = self.build().unwrap();
        PullRequestComment {
            repository: repo,
            pr_number,
            text,
            author,
        }
    }
}

impl From<&str> for PullRequestComment {
    fn from(value: &str) -> Self {
        Self {
            repository: default_repo_name(),
            author: default_user(),
            pr_number: default_pr_number(),
            text: value.to_string(),
        }
    }
}

impl From<CommentBuilder> for PullRequestComment {
    fn from(value: CommentBuilder) -> Self {
        value.create()
    }
}

pub fn comment(text: &str) -> CommentBuilder {
    CommentBuilder::default().text(text.to_string())
}

pub fn suite_success() -> CheckSuite {
    CheckSuite {
        status: CheckSuiteStatus::Success,
    }
}

pub fn suite_failure() -> CheckSuite {
    CheckSuite {
        status: CheckSuiteStatus::Failure,
    }
}

pub fn suite_pending() -> CheckSuite {
    CheckSuite {
        status: CheckSuiteStatus::Pending,
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct WorkflowStarted {
    #[builder(default = "default_repo_name()")]
    repo: GithubRepoName,
    #[builder(default = "\"workflow-name\".to_string()")]
    name: String,
    branch: String,
    commit_sha: String,
    #[builder(default = "Some(1)")]
    run_id: Option<u64>,
    #[builder(default = "\"https://workflow.com\".to_string()")]
    url: String,
}

impl WorkflowStartedBuilder {
    pub fn create(self) -> event::WorkflowStarted {
        let WorkflowStarted {
            repo,
            name,
            branch,
            commit_sha,
            run_id,
            url,
        } = self.build().unwrap();

        event::WorkflowStarted {
            repository: repo,
            name,
            branch,
            commit_sha: CommitSha(commit_sha),
            run_id,
            url,
        }
    }
}

impl From<WorkflowStartedBuilder> for event::WorkflowStarted {
    fn from(value: WorkflowStartedBuilder) -> Self {
        value.create()
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct WorkflowCompleted {
    #[builder(default = "default_repo_name()")]
    repo: GithubRepoName,
    #[builder(default = "1")]
    run_id: u64,
    status: WorkflowStatus,
}

impl WorkflowCompletedBuilder {
    pub fn create(self) -> event::WorkflowCompleted {
        let crate::tests::event::WorkflowCompleted {
            repo,
            run_id,
            status,
        } = self.build().unwrap();

        event::WorkflowCompleted {
            repository: repo,
            run_id,
            status,
        }
    }
}

impl From<WorkflowCompletedBuilder> for event::WorkflowCompleted {
    fn from(value: WorkflowCompletedBuilder) -> Self {
        value.create()
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct CheckSuiteCompleted {
    #[builder(default = "default_repo_name()")]
    repo: GithubRepoName,
    branch: String,
    commit_sha: String,
}

impl CheckSuiteCompletedBuilder {
    pub fn create(self) -> event::CheckSuiteCompleted {
        let crate::tests::event::CheckSuiteCompleted {
            repo,
            branch,
            commit_sha,
        } = self.build().unwrap();

        event::CheckSuiteCompleted {
            repository: repo,
            branch,
            commit_sha: CommitSha(commit_sha),
        }
    }
}

impl From<CheckSuiteCompletedBuilder> for event::CheckSuiteCompleted {
    fn from(value: CheckSuiteCompletedBuilder) -> Self {
        value.create()
    }
}
