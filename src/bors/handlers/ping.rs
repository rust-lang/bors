use std::fmt::Write;
use std::sync::Arc;

use crate::PgDbClient;
use crate::bors::Comment;
use crate::bors::RepositoryState;
use crate::github::PullRequestNumber;

pub(super) async fn command_ping(
    repo: Arc<RepositoryState>,
    db: &PgDbClient,
    pr_number: PullRequestNumber,
) -> anyhow::Result<()> {
    let mut msg = "Pong :ping_pong:!".to_string();

    // We don't embed the git version automatically, e.g. through the
    // `git-version` crate, because for production we build bors in Docker anyway, where there
    // is no git repo.
    let git_version = option_env!("GIT_VERSION").unwrap_or_else(|| "unknown");
    writeln!(msg, "\n\nbors build: `{git_version}`").unwrap();
    repo.client
        .post_comment(pr_number, Comment::new(msg), db)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::tests::{BorsTester, run_test};

    #[sqlx::test]
    async fn ping_command(pool: sqlx::PgPool) {
        run_test(pool, async |ctx: &mut BorsTester| {
            ctx.post_comment("@bors ping").await?;
            insta::assert_snapshot!(ctx.get_next_comment_text(()).await?, @"
            Pong :ping_pong:!

            bors build: `unknown`
            ");
            Ok(())
        })
        .await;
    }
}
