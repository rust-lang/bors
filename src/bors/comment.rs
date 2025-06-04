use serde::Serialize;

use crate::{
    database::{WorkflowModel, WorkflowStatus},
    github::CommitSha,
};

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

pub fn try_build_succeeded_comment(workflows: &[WorkflowModel], commit_sha: CommitSha) -> Comment {
    use std::fmt::Write;

    let mut text = String::from(":sunny: Try build successful");

    // If there is only a single workflow (the common case), compress the output
    // so that it doesn't take so much space
    if workflows.len() == 1 {
        writeln!(text, " ([{}]({}))", workflows[0].name, workflows[0].url).unwrap();
    } else {
        let workflows_status = list_workflows_status(workflows);
        writeln!(text, "\n{workflows_status}").unwrap();
    }
    writeln!(text, "Build commit: {commit_sha} (`{commit_sha}`)").unwrap();

    Comment {
        text,
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
    let mut try_build_cancelled_comment =
        r#"Try build cancelled. Cancelled workflows:"#.to_string();
    for url in workflow_urls {
        try_build_cancelled_comment += format!("\n- {}", url).as_str();
    }
    Comment::new(try_build_cancelled_comment)
}

pub fn workflow_failed_comment(workflows: &[WorkflowModel]) -> Comment {
    let workflows_status = list_workflows_status(workflows);
    Comment::new(format!(
        r#":broken_heart: Test failed
{}"#,
        workflows_status
    ))
}

pub fn try_build_started_comment(
    head_sha: &CommitSha,
    merge_sha: &CommitSha,
    bot_prefix: &str,
) -> Comment {
    Comment::new(format!(
        ":hourglass: Trying commit {head_sha} with merge {merge_sha}…

To cancel the try build, run the command `{bot_prefix} try cancel`."
    ))
}

pub fn merge_conflict_comment(branch: &str) -> Comment {
    let message = format!(
        r#":lock: Merge conflict

This pull request and the master branch diverged in a way that cannot
 be automatically merged. Please rebase on top of the latest master
 branch, and let the reviewer approve again.

<details><summary>How do I rebase?</summary>

Assuming `self` is your fork and `upstream` is this repository,
 you can resolve the conflict following these steps:

1. `git checkout {branch}` *(switch to your branch)*
2. `git fetch upstream master` *(retrieve the latest master)*
3. `git rebase upstream/master -p` *(rebase on top of it)*
4. Follow the on-screen instruction to resolve conflicts (check `git status` if you got lost).
5. `git push self {branch} --force-with-lease` *(update this PR)*

You may also read
 [*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html)
 for a short tutorial.

Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub.
 It uses `git merge` instead of `git rebase` which makes the PR commit history more difficult to read.

Sometimes step 4 will complete without asking for resolution. This is usually due to difference between how `Cargo.lock` conflict is
handled during merge and rebase. This is normal, and you should still perform step 5 to update this PR.

</details>
"#
    );
    Comment::new(message)
}

fn list_workflows_status(workflows: &[WorkflowModel]) -> String {
    workflows
        .iter()
        .map(|w| {
            format!(
                "- [{}]({}) {}",
                w.name,
                w.url,
                if w.status == WorkflowStatus::Success {
                    ":white_check_mark:"
                } else {
                    ":x:"
                }
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}
