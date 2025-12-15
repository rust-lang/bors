use crate::bors::RepositoryState;
use crate::bors::{Comment, format_help};
use crate::github::PullRequestNumber;
use std::sync::Arc;

pub(super) async fn command_help(
    repo: Arc<RepositoryState>,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    let help = format_help();

    repo.client
        .post_comment(pr_number, Comment::new(help.to_string()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn help_command(pool: sqlx::PgPool) {
        run_test(pool, async |tester: &mut BorsTester| {
            tester.post_comment("@bors help").await?;
            insta::assert_snapshot!(tester.get_next_comment_text(()).await?, @r"
            You can use the following commands:

            ## PR management
            - `r+ [p=<priority>] [rollup=<never|iffy|maybe|always>]`: Approve this PR on your behalf
                - Optionally, you can specify the `<priority>` of the PR and if it is eligible for rollups (`<rollup>)`.
            - `r=<user> [p=<priority>] [rollup=<never|iffy|maybe|always>]`: Approve this PR on behalf of `<user>`
                - Optionally, you can specify the `<priority>` of the PR and if it is eligible for rollups (`<rollup>)`.
                - You can pass a comma-separated list of GitHub usernames.
            - `r-`: Unapprove this PR
            - `p=<priority>` or `priority=<priority>`: Set the priority of this PR
            - `rollup=<never|iffy|maybe|always>`: Set the rollup status of the PR
            - `rollup`: Short for `rollup=always`
            - `rollup-`: Short for `rollup=maybe`
            - `delegate=<try|review>`: Delegate permissions for running try builds or approving to the PR author
                - `try` allows the PR author to start try builds.
                - `review` allows the PR author to both start try builds and approve the PR.
            - `delegate+`: Delegate approval permissions to the PR author
                - Shortcut for `delegate=review`
            - `delegate-`: Remove any previously granted permission delegation
            - `try [parent=<parent>] [job|jobs=<jobs>]`: Start a try build.
                - Optionally, you can specify a `<parent>` SHA with which will the PR be merged. You can specify `parent=last` to use the same parent SHA as the previous try build.
                - Optionally, you can select a comma-separated list of CI `<jobs>` to run in the try build.
            - `try cancel`: Cancel a running try build
            - `retry`: Clear a failed auto build status from an approved PR. This will cause the merge queue to attempt to start a new auto build and retry merging the PR again.
            - `info`: Get information about the current PR

            ## Repository management
            - `treeclosed=<priority>`: Close the tree for PRs with priority less than `<priority>`
            - `treeclosed-` or `treeopen`: Open the repository tree for merging

            ## Meta commands
            - `ping`: Check if the bot is alive
            - `help`: Print this help message
            ");
            Ok(())
        })
        .await;
    }
}
