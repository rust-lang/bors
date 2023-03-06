use std::collections::HashMap;
use std::sync::Mutex;

use axum::async_trait;

use crate::github::{GithubRepoName, PullRequest};
use crate::handlers::RepositoryClient;

pub struct TestRepositoryClient {
    name: GithubRepoName,
    comments: Mutex<HashMap<u64, Vec<String>>>,
}

impl TestRepositoryClient {
    pub fn check_comments(&self, pr_number: u64, comments: &[&str]) {
        assert_eq!(
            self.comments.lock().unwrap().get(&pr_number).unwrap(),
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
    }
}

#[async_trait]
impl RepositoryClient for TestRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
        self.comments
            .lock()
            .unwrap()
            .entry(pr.number)
            .or_default()
            .push(text.to_string());
        Ok(())
    }
}
