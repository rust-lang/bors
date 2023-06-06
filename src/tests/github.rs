use derive_builder::Builder;

use crate::github::{Branch as GHBranch, PullRequest};
use crate::tests::event::default_pr_number;

#[derive(Builder)]
pub struct PR {
    #[builder(default = "default_pr_number()")]
    number: u64,
    #[builder(default = "\"head-label\".to_string()")]
    head_label: String,
    #[builder(default = "self.default_head()")]
    head: GHBranch,
    #[builder(default = "self.default_base()")]
    base: GHBranch,
    #[builder(default = "\"PR title\".to_string()")]
    title: String,
    #[builder(default = "\"PR message\".to_string()")]
    message: String,
}

impl PRBuilder {
    pub fn create(&mut self) -> PullRequest {
        let PR {
            number,
            head_label,
            head,
            base,
            title,
            message,
        } = self.build().unwrap();

        PullRequest {
            number: number.into(),
            head_label,
            head,
            base,
            title,
            message,
        }
    }

    fn default_head(&self) -> GHBranch {
        BranchBuilder::default()
            .name(format!(
                "pr-{}",
                self.number.unwrap_or_else(default_pr_number)
            ))
            .sha("pr-sha".to_string())
            .create()
    }

    fn default_base(&self) -> GHBranch {
        BranchBuilder::default()
            .name("main-branch".to_string())
            .sha("main-sha".to_string())
            .create()
    }
}

#[derive(Builder)]
pub struct Branch {
    #[builder(default = "\"branch-1\".to_string()")]
    name: String,
    #[builder(default = "\"sha-1\".to_string()")]
    sha: String,
}

impl BranchBuilder {
    pub fn create(&mut self) -> GHBranch {
        let Branch { name, sha } = self.build().unwrap();
        GHBranch {
            name,
            sha: sha.into(),
        }
    }
}
