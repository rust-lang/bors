use std::collections::HashMap;

use axum::async_trait;
use octocrab::models::pulls::Merge;

use crate::github::api::operations::MergeError;
use crate::github::{CommitSha, GithubRepoName, PullRequest};
use crate::handlers::RepositoryClient;

pub struct TestRepositoryClient {
    name: GithubRepoName,
    comments: HashMap<u64, Vec<String>>,
    pub merge_branches_fn: Box<dyn Fn() -> Result<CommitSha, MergeError> + Send>,
}

impl TestRepositoryClient {
    pub fn get_comment(&self, pr_number: u64, comment_index: usize) -> &str {
        &self.comments.get(&pr_number).unwrap()[comment_index]
    }
    pub fn check_comments(&self, pr_number: u64, comments: &[&str]) {
        assert_eq!(
            self.comments.get(&pr_number).unwrap(),
            &comments
                .into_iter()
                .map(|&s| String::from(s))
                .collect::<Vec<_>>()
        );
    }
}

pub fn test_client() -> TestRepositoryClient {
    TestRepositoryClient {
        comments: Default::default(),
        name: GithubRepoName::new("foo", "bar"),
        merge_branches_fn: Box::new(|| Ok(CommitSha("foo".to_string()))),
    }
}

#[async_trait]
impl RepositoryClient for TestRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn post_comment(&mut self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
        self.comments
            .entry(pr.number)
            .or_default()
            .push(text.to_string());
        Ok(())
    }

    async fn set_branch_to_sha(&mut self, branch: &str, sha: &CommitSha) -> anyhow::Result<()> {
        Ok(())
    }

    async fn merge_branches(
        &mut self,
        base: &str,
        head: &CommitSha,
        commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        (self.merge_branches_fn)()
    }
}
