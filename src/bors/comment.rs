use serde::Serialize;

use crate::github::CommitSha;

/// A comment that can be posted to a pull request.
pub struct Comment {
    text: String,
    metadata: Option<CommentMetadata>,
}

#[derive(Serialize)]
#[serde(tag = "type")]
pub enum CommentMetadata {
    TryBuildCompleted { merge_sha: String },
}

impl Comment {
    pub fn new(text: String) -> Self {
        Self {
            text,
            metadata: None,
        }
    }

    pub fn render(&self) -> String {
        if let Some(metadata) = &self.metadata {
            return format!(
                "{}\n<!-- homu: {} -->",
                self.text,
                serde_json::to_string(metadata).unwrap()
            );
        }
        self.text.clone()
    }
}

pub fn try_build_succeeded_comment(workflow_list: String, commit_sha: CommitSha) -> Comment {
    Comment {
        text: format!(
            r#":sunny: Try build successful
{}
Build commit: {} (`{}`)"#,
            workflow_list, commit_sha, commit_sha
        ),
        metadata: Some(CommentMetadata::TryBuildCompleted {
            merge_sha: commit_sha.to_string(),
        }),
    }
}
