use std::collections::HashMap;

use crate::config::RepositoryConfig;
use axum::async_trait;
use derive_builder::Builder;

use super::permissions::AllPermissions;
use crate::github::api::client::PullRequestNumber;
use crate::github::api::operations::MergeError;
use crate::github::api::RepositoryState;
use crate::github::{Branch, CommitSha, GithubRepoName, PullRequest};
use crate::handlers::RepositoryClient;
use crate::permissions::PermissionResolver;

#[derive(Builder)]
#[builder(pattern = "owned")]
pub struct Repo {
    #[builder(default)]
    name: Option<GithubRepoName>,
    #[builder(default = "Box::new(AllPermissions)")]
    permission_resolver: Box<dyn PermissionResolver>,
    #[builder(default)]
    config: Option<RepositoryConfig>,
}

impl RepoBuilder {
    pub fn create(self) -> RepositoryState<TestRepositoryClient> {
        let Repo {
            name,
            permission_resolver,
            config,
        } = self.build().unwrap();

        let name = name.unwrap_or_else(|| GithubRepoName::new("owner", "name"));
        RepositoryState {
            repository: name.clone(),
            client: TestRepositoryClient {
                comments: Default::default(),
                name,
                merge_branches_fn: Box::new(|| Ok(CommitSha("foo".to_string()))),
            },
            permissions_resolver: permission_resolver,
            config: config.unwrap_or_else(|| RepositoryConfig { checks: vec![] }),
        }
    }
}

pub struct TestRepositoryClient {
    pub name: GithubRepoName,
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

#[async_trait]
impl RepositoryClient for TestRepositoryClient {
    fn repository(&self) -> &GithubRepoName {
        &self.name
    }

    async fn get_pull_request(&mut self, pr: PullRequestNumber) -> anyhow::Result<PullRequest> {
        Ok(PullRequest {
            number: pr.0,
            head_label: "label".to_string(),
            head: Branch {
                name: "head".to_string(),
                sha: CommitSha("head-sha".to_string()),
            },
            base: Branch {
                name: "base".to_string(),
                sha: CommitSha("base".to_string()),
            },
            title: "title".to_string(),
            message: "message".to_string(),
        })
    }

    async fn post_comment(&mut self, pr: PullRequestNumber, text: &str) -> anyhow::Result<()> {
        self.comments
            .entry(pr.0)
            .or_default()
            .push(text.to_string());
        Ok(())
    }

    async fn set_branch_to_sha(&mut self, _branch: &str, _sha: &CommitSha) -> anyhow::Result<()> {
        Ok(())
    }

    async fn merge_branches(
        &mut self,
        _base: &str,
        _head: &CommitSha,
        _commit_message: &str,
    ) -> Result<CommitSha, MergeError> {
        (self.merge_branches_fn)()
    }
}
