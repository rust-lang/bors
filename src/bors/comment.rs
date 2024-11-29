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

pub fn try_build_in_progress_comment() -> Comment {
    Comment::new(":exclamation: A try build is currently in progress. You can cancel it using @bors try cancel.".to_string())
}

pub fn cant_find_last_parent_comment() -> Comment {
    Comment::new(":exclamation: There was no previous build. Please set an explicit parent or remove the `parent=last` argument to use the default parent.".to_string())
}

pub fn no_try_build_in_progress_comment() -> Comment {
    Comment::new(":exclamation: There is currently no try build in progress.".to_string())
}

pub fn unclean_try_build_cancelled_comment() -> Comment {
    Comment::new(
        "Try build was cancelled. It was not possible to cancel some workflows.".to_string(),
    )
}

pub fn try_build_cancelled_comment(workflow_urls: impl Iterator<Item = String>) -> Comment {
    let mut try_build_cancelled_comment = r#"Try build cancelled.
Cancelled workflows:"#
        .to_string();
    for url in workflow_urls {
        try_build_cancelled_comment += format!("\n- {}", url).as_str();
    }
    Comment::new(try_build_cancelled_comment)
}
