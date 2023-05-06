use derive_builder::Builder;

use crate::bors::event::PullRequestComment;
use crate::github::{GithubRepoName, GithubUser};
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
