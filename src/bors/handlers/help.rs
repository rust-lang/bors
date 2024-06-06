use std::sync::Arc;

use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequest;

const HELP_MESSAGE: &str = r#"
- try: Execute the `try` CI workflow on this PR (without approving it for a merge).
- try cancel: Stop try builds on current PR.
- ping: pong
- help: Print this help message
"#;

pub(super) async fn command_help(
    repo: Arc<RepositoryState>,
    pr: &PullRequest,
) -> anyhow::Result<()> {
    repo.client
        .post_comment(pr.number, Comment::new(HELP_MESSAGE.to_string()))
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::bors::handlers::help::HELP_MESSAGE;
    use crate::tests::mocks::run_test;

    #[sqlx::test]
    async fn help_command(pool: sqlx::PgPool) {
        run_test(pool, |mut tester| async {
            tester.post_comment("@bors help").await?;
            assert_eq!(tester.get_comment().await?, HELP_MESSAGE);
            Ok(tester)
        })
        .await;
    }
}
