use crate::bors::event;
use derive_builder::Builder;

use crate::bors::event::PullRequestComment;
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

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct WorkflowStarted {
    #[builder(default = "default_repo_name()")]
    repo: GithubRepoName,
    #[builder(default = "\"workflow-name\".to_string()")]
    name: String,
    branch: String,
    commit_sha: String,
    #[builder(default = "1")]
    run_id: u64,
    #[builder(default = "1")]
    check_suite_id: u64,
}

impl WorkflowStartedBuilder {
    pub fn create(self) -> event::WorkflowStarted {
        let WorkflowStarted {
            repo,
            name,
            branch,
            commit_sha,
            run_id,
            check_suite_id,
        } = self.build().unwrap();

        event::WorkflowStarted {
            repository: repo,
            name,
            branch,
            commit_sha: CommitSha(commit_sha),
            workflow_run_id: run_id,
            check_suite_id,
        }
    }
}

impl From<WorkflowStartedBuilder> for event::WorkflowStarted {
    fn from(value: WorkflowStartedBuilder) -> Self {
        value.create()
    }
}
