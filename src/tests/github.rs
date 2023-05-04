use crate::github::{Branch as GHBranch, GithubUser, PullRequest};
use derive_builder::Builder;

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct PR {
    #[builder(default = "1")]
    number: u64,
    #[builder(default)]
    head_label: String,
    #[builder(default)]
    head: BranchBuilder,
    #[builder(default)]
    base: BranchBuilder,
    #[builder(default)]
    title: String,
    #[builder(default)]
    message: String,
}

impl PRBuilder {
    pub fn create(self) -> PullRequest {
        let PR {
            number,
            head_label,
            head,
            base,
            title,
            message,
        } = self.build().unwrap();

        PullRequest {
            number,
            head_label,
            head: head.create(),
            base: base.create(),
            title,
            message,
        }
    }
}

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Branch {
    #[builder(default)]
    name: String,
    #[builder(default)]
    sha: String,
}

impl BranchBuilder {
    pub fn create(self) -> GHBranch {
        let Branch { name, sha } = self.build().unwrap();
        GHBranch {
            name,
            sha: sha.into(),
        }
    }
}

pub fn create_user(username: &str) -> GithubUser {
    GithubUser {
        username: username.to_string(),
        html_url: format!("https://github.com/{username}").parse().unwrap(),
    }
}
