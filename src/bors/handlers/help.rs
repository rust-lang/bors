use crate::PgDbClient;
use crate::bors::RepositoryState;
use crate::bors::{Comment, format_help};
use crate::github::PullRequestNumber;
use std::sync::Arc;

pub(super) async fn command_help(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    let help = format_help();

    repo.client
        .post_comment(pr_number, Comment::new(help.to_string()), db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn help_command(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors help").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"

            You can use the following commands:

            ## PR management: Approval
            These commands can be arbitrarily chained into one single command.
            For example: `@bors r+ squash rollup` will squash the PR, approve it, and mark it rollup-able.

            - `r+`: Approve this PR on your behalf
            - `r=<user>`: Approve this PR on behalf of `<user>`
            - `p=<priority>`: Set the priority of this PR. Higher number is higher priority.
            - `rollup=<never|iffy|maybe|always>`: Set the rollup status of the PR.
            - `rollup`: Short for `rollup=always`
            - `rollup-`: Short for `rollup=maybe`
            - `force`: The default is for approvals to remain tentative until PR CI succeeds. Pass `force` to approve the PR immediately.
                - Can only be passed if `r+` or `r=` is passed.
            - `note=<note>`: Attach a `<note>` to the PR that will be displayed on the queue page.
            - `squash [msg|message=<commit-message>|description]`: Squash the commits of a PR into a single commit.
                - Optionally, you can specify a `<commit-message>` for the created commit. If not specified, the commit messages of all squashed commits will be combined.
                - If you specify `msg=description`, then the PR title and body will be used as the squashed commit message.

            ## PR management: Other
            - `r-`: Unapprove this PR
            - `delegate[=<user>] [try|review]`: Delegate permissions for running try builds or approving to the PR author or the specified `<user>`.
                - If no `<user>` is specified, the PR author is delegated the permission.
                - The default permission is `review`.
                - `try` allows the delegatee to start try builds.
                - `review` allows the delegatee to both start try builds and approve the PR.
            - `delegate` | `delegate+`: Delegate approval permissions to the PR author
                - Shortcut for `delegate review`
            - `delegate-`: Remove any previously granted permission delegation
            - `try [parent=<parent>] [job|jobs=<jobs>] [nolimit]`: Start a try build.
                - Optionally, you can specify a `<parent>` SHA with which will the PR be merged. You can specify `parent=last` to use the same parent SHA as the previous try build.
                - Optionally, you can select a comma-separated list of CI `<jobs>` to run in the try build.
                  Examples:
                    - `@bors try jobs=x86_64-gnu-nopt`
                    - `@bors try jobs=x86_64-gnu-nopt,x86_64-mingw-*` (matches `x86_64-gnu-nopt` and jobs starting with `x86_64-mingw-`)
                - Optionally, you can turn off the limit on the number of executed try jobs with `nolimit`.
            - `try cancel`: Cancel a running try build on the current PR.
            - `retry`: Clear a failed auto build status from an approved PR. This will cause the merge queue to eventually attempt to merge the PR again.
            - `cancel` | `yield`: Cancel a running auto build on the current PR.
            - `info`: Get information about the current PR

            ## Repository management
            - `treeclosed=<priority> [reason]`: Close the tree for PRs with priority less than `<priority>`. Optionally, you can specify a `<reason>`.
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
