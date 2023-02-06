use std::future::Future;

use anyhow::Context;
use octocrab::models::pulls::PullRequest;
use tokio::sync::mpsc;

use crate::command::parser::{parse_commands, CommandParseError};
use crate::command::BorsCommand;
use crate::github::api::operations::{merge_branches, post_pr_comment, MergeError};
use crate::github::api::{GithubAppClient, RepositoryClient};
use crate::github::event::{PullRequestComment, WebhookEvent};
use crate::github::GithubUser;

pub type WebhookSender = mpsc::Sender<WebhookEvent>;

/// Asynchronous process that receives webhooks and reacts to them.
pub fn github_webhook_process(
    mut client: GithubAppClient,
) -> (WebhookSender, impl Future<Output = ()>) {
    let (tx, mut rx) = mpsc::channel::<WebhookEvent>(1024);

    let service = async move {
        while let Some(event) = rx.recv().await {
            log::trace!("Received webhook: {event:#?}");

            match event {
                WebhookEvent::Comment(event) => match client.get_repo_client(&event.repository) {
                    Some(repo) => {
                        if let Err(error) = handle_comment(&client, repo, event).await {
                            log::warn!("Error occured while handling event: {error:?}");
                        }
                    }
                    None => {
                        log::warn!("Repository {} not found", event.repository);
                    }
                },
                WebhookEvent::InstallationsChanged => {
                    log::info!("Reloading installation repositories");
                    if let Err(error) = client.reload_repositories().await {
                        log::error!("Could not reload installation repositories: {error:?}");
                    }
                }
            }
        }
    };
    (tx, service)
}

async fn handle_comment(
    client: &GithubAppClient,
    repo: &RepositoryClient,
    comment: PullRequestComment,
) -> anyhow::Result<()> {
    // We want to ignore comments made by this bot
    if client.is_comment_internal(&comment) {
        log::trace!("Ignoring comment {comment:?} because it was authored by this bot");
        return Ok(());
    }

    let pr_number = comment.pr_number;
    let commands = parse_commands(&comment.text);
    let pull_request = repo
        .client()
        .pulls(&repo.name().owner, &repo.name().name)
        .get(pr_number)
        .await
        .map_err(|error| {
            anyhow::anyhow!("Could not get PR {}/{pr_number}: {error:?}", repo.name())
        })?;

    log::info!(
        "Received comment at https://github.com/{}/{}/issues/{}, commands: {:?}",
        repo.name().owner,
        repo.name().name,
        pr_number,
        commands
    );

    let service = BorsService;
    for command in commands {
        match command {
            Ok(command) => {
                let result = match command {
                    BorsCommand::Ping => service.ping(repo, pull_request.clone()).await,
                    BorsCommand::Try => {
                        service
                            .try_merge(repo, &comment.author, pull_request.clone())
                            .await
                    }
                };
                if result.is_err() {
                    return result.context("Cannot execute Bors command");
                }
            }
            Err(error) => {
                let error_msg = match error {
                    CommandParseError::MissingCommand => "Missing command.".to_string(),
                    CommandParseError::UnknownCommand(command) => {
                        format!(r#"Unknown command "{command}"."#)
                    }
                };

                post_pr_comment(repo, pr_number, &error_msg)
                    .await
                    .context("Could not reply to PR comment")?;
            }
        }
    }
    Ok(())
}

struct BorsService;

impl BorsService {
    async fn ping(&self, repo: &RepositoryClient, pr: PullRequest) -> anyhow::Result<()> {
        log::debug!("Executing ping");
        post_pr_comment(repo, pr.number, "Pong 🏓!").await?;
        Ok(())
    }

    async fn try_merge(
        &self,
        repo: &RepositoryClient,
        author: &GithubUser,
        pr: PullRequest,
    ) -> anyhow::Result<()> {
        log::debug!("Executing try on {}/{}", repo.name(), pr.number);

        let branch_label = pr.head.label.unwrap_or_else(|| "<unknown>".to_string());
        let merge_msg = format!(
            "Auto merge of #{} - {}, r={}",
            pr.id, branch_label, author.username
        );

        let base = pr.base.ref_field;
        let head = pr.head.ref_field;

        // Just merge the PR directly right now. Let's hope that tests pass :)
        let result = merge_branches(repo, &base, &head, &merge_msg).await;
        log::debug!("Result of merge: {result:?}");

        let response = match result {
            Ok(_) => None,
            Err(error) => match error {
                MergeError::NotFound => {
                    Some(format!("Base `{base}` or head `{head}` were not found."))
                }
                MergeError::Conflict => Some(merge_conflict_message(&head)),
                MergeError::AlreadyMerged => Some("This branch was already merged.".to_string()),
                result @ MergeError::Unknown { .. } | result @ MergeError::NetworkError(_) => {
                    log::error!(
                        "Failed to merge {branch_label} into head on {}: {:?}",
                        repo.name(),
                        result
                    );
                    Some("An error has occurred. Please try again later.".to_string())
                }
            },
        };
        if let Some(message) = response {
            post_pr_comment(repo, pr.number, &message)
                .await
                .context("Cannot send PR comment")?;
        }
        Ok(())
    }
}

fn merge_conflict_message(branch: &str) -> String {
    format!(
        r#"Merge conflict

This pull request and the head branch diverged in a way that cannot
be automatically merged. Please rebase on top of the latest master
branch, and let the reviewer approve again.

<details><summary>How do I rebase?</summary>

Assuming `self` is your fork and `upstream` is this repository,
you can resolve the conflict following these steps:

1. `git checkout {branch}` *(switch to your branch)*
2. `git fetch upstream master` *(retrieve the latest master)*
3. `git rebase upstream/master -p` *(rebase on top of it)*
4. Follow the on-screen instruction to resolve conflicts
 (check `git status` if you got lost).
5. `git push self {branch} --force-with-lease` *(update this PR)*

You may also read
[*Git Rebasing to Resolve Conflicts* by Drew Blessing](http://blessing.io/git/git-rebase/open-source/2015/08/23/git-rebasing-to-resolve-conflicts.html) # noqa
for a short tutorial.

Please avoid the ["**Resolve conflicts**" button](https://help.github.com/articles/resolving-a-merge-conflict-on-github/) on GitHub. # noqa
It uses `git merge` instead of `git rebase` which makes the PR commit # noqa
history more difficult to read.

Sometimes step 4 will complete without asking for resolution. This is
usually due to difference between how `Cargo.lock` conflict is
handled during merge and rebase. This is normal, and you should still # noqa
perform step 5 to update this PR.

</details>
"#
    )
}
