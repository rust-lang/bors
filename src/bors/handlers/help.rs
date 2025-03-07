use crate::bors::command::{Approver, BorsCommand};
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequest;
use std::sync::Arc;

pub(super) async fn command_help(
    repo: Arc<RepositoryState>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    let help = [
        BorsCommand::Approve {
            approver: Approver::Myself,
            priority: None,
        },
        BorsCommand::Approve {
            approver: Approver::Specified("".to_string()),
            priority: None,
        },
        BorsCommand::Unapprove,
        BorsCommand::SetPriority(0),
        BorsCommand::Delegate,
        BorsCommand::Undelegate,
        BorsCommand::Try {
            parent: None,
            jobs: vec![],
        },
        BorsCommand::TryCancel,
        BorsCommand::Ping,
        BorsCommand::Help,
    ]
    .into_iter()
    .map(|help| format!("- {}", get_command_help(help)))
    .collect::<Vec<_>>()
    .join("\n");

    repo.client
        .post_comment(pr.number, Comment::new(help))
        .await?;
    Ok(())
}

fn get_command_help(command: BorsCommand) -> String {
    // !!! When modifying this match, also update the command list above (in [`command_help`]) !!!
    let help = match command {
        BorsCommand::Approve { approver: Approver::Myself, .. } => {
            "`r+ [p=<priority>]`: Approve this PR. Optionally, you can specify a `<priority>`."
        }
        BorsCommand::Approve {approver: Approver::Specified(_), ..} => {
            "`r=<user> [p=<priority>]`: Approve this PR on behalf of `<user>`. Optionally, you can specify a `<priority>`."
        }
        BorsCommand::Unapprove => {
            "`r-`: Unapprove this PR"
        }
        BorsCommand::SetPriority(_) => {
            "`p=<priority>`: Set the priority of this PR"
        }
        BorsCommand::Delegate => {
            "`delegate+`: Delegate approval authority to the PR author"
        }
        BorsCommand::Undelegate => {
            "`delegate-`: Remove any previously granted delegation"
        }
        BorsCommand::Help => {
            "`help`: Print this help message"
        }
        BorsCommand::Ping => {
            "`ping`: Check if the bot is alive"
        }
        BorsCommand::Try { .. } => {
            "`try [parent=<parent>] [jobs=<jobs>]`: Start a try build. Optionally, you can specify a `<parent>` SHA or a list of `<jobs>` to run"
        }
        BorsCommand::TryCancel => {
            "`try cancel`: Cancel a running try build"
        }
    };
    help.to_string()
}

#[cfg(test)]
mod tests {
    use crate::tests::mocks::run_test;

    #[sqlx::test]
    async fn help_command(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors help").await?;
            insta::assert_snapshot!(tester.get_comment().await?, @r"
            - `r+ [p=<priority>]`: Approve this PR. Optionally, you can specify a `<priority>`.
            - `r=<user> [p=<priority>]`: Approve this PR on behalf of `<user>`. Optionally, you can specify a `<priority>`.
            - `r-`: Unapprove this PR
            - `p=<priority>`: Set the priority of this PR
            - `delegate+`: Delegate approval authority to the PR author
            - `delegate-`: Remove any previously granted delegation
            - `try [parent=<parent>] [jobs=<jobs>]`: Start a try build. Optionally, you can specify a `<parent>` SHA or a list of `<jobs>` to run
            - `try cancel`: Cancel a running try build
            - `ping`: Check if the bot is alive
            - `help`: Print this help message
            ");
            Ok(tester)
        })
        .await;
    }
}
