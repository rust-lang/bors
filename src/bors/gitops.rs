use crate::github::{CommitSha, GithubRepoName};
use anyhow::Context;
use secrecy::SecretString;
use std::path::PathBuf;
use std::process::Command;

/// Represents a git binary.
#[derive(Clone)]
pub struct Git {
    git: PathBuf,
}

impl Git {
    /// Try to locate a git binary and execute it.
    /// Returns `Git` if it worked.
    pub fn try_init() -> anyhow::Result<Self> {
        let path = which::which("git").context("git was not found")?;
        if Command::new(&path)
            .arg("-v")
            .status()
            .context("Cannot execute git")?
            .success()
        {
            Ok(Git { git: path })
        } else {
            Err(anyhow::anyhow!("Cannot execute git at `{path:?}`"))
        }
    }

    #[cfg(test)]
    pub fn from_path(path: PathBuf) -> Self {
        Self { git: path }
    }

    /// Pushes a commit from the source repository to the target repository.
    /// Note: to achieve higher performance, this does not fetch or push any trees!
    /// It can be used only to push a single commit between two repositories.
    pub async fn transfer_commit_between_repositories(
        &self,
        source_repo: &GithubRepoName,
        target_repo: &GithubRepoName,
        commit: &CommitSha,
        target_branch: &str,
        token: SecretString,
    ) -> anyhow::Result<()> {
        use secrecy::ExposeSecret;

        // What we want to do here is to push a commit A from repo R1 (source) to repo R2 (target)
        // as quickly as possible, and in a stateless way.
        // Previously, we used libgit2 to do essentially `fetch --depth=1` followed by a `git push`.
        // However, this is wasteful, because we do not actually need to download any blobs or
        // trees.
        // For the transfer, we simply need to transfer a simply commit between those two
        // repositories.
        // So we first do a blob/treeless clone of the source repository, and then push a single
        // commit to the target repository. git will use its unshallowing logic to lazily download
        // the pushed commit from the source repo, and then push it to the target repo.

        // Create a temporary directory for the local repository
        let temp_dir = tempfile::tempdir()?;
        let root_path = temp_dir.path();

        let source_repo_url = format!("https://github.com/{source_repo}.git");

        run_command(
            tokio::process::Command::new(&self.git)
                .kill_on_drop(true)
                .current_dir(root_path)
                .arg("init")
                .arg("--bare"),
        )
        .await
        .context("Cannot perform git init")?;

        // It **should** be much faster to do a partial clone than a fetch with depth=1.
        // However, on the production server, the partial clone of rust-lang/rust seems to choke :(
        // So we use the fetch as an alternative.
        tracing::debug!("Fetching commit");
        run_command(
            tokio::process::Command::new(&self.git)
                .kill_on_drop(true)
                .current_dir(root_path)
                .arg("fetch")
                .arg("--depth=1")
                // Note: using --filter=tree:0 makes the fetch much faster, but the resulting push
                // becomes MUCH slower :(
                .arg(source_repo_url)
                .arg(commit.as_ref()),
        )
        .await
        .context("Cannot perform git clone")?;

        let target_branch = format!("refs/heads/{target_branch}");
        // Create the refspec: push the commit to the target branch
        // The `+` sign says that it is a force push
        let refspec = format!("+{commit}:{target_branch}");
        // Embed the token into the push URL
        let target_repo_url = format!(
            "https://bors:{}@github.com/{target_repo}.git",
            token.expose_secret()
        );

        tracing::debug!(
            "Pushing commit to https://bors:<token>@github.com/{target_repo}.git, refspec `{refspec}`"
        );

        // And then push the commit
        run_command(
            tokio::process::Command::new(&self.git)
                .kill_on_drop(true)
                .current_dir(root_path)
                // Do not store the token on disk
                .arg("-c")
                .arg("credential.helper=")
                .arg("push")
                .arg(&target_repo_url)
                .arg(refspec),
        )
        .await
        .context("Cannot perform git push")?;
        Ok(())
    }
}

async fn run_command(cmd: &mut tokio::process::Command) -> anyhow::Result<()> {
    // Use status instead of output, so that we stream the output directly into logs.
    // If we buffered it, then we would not print anything in case of a timeout.
    let status = cmd.status().await?;
    if !status.success() {
        Err(anyhow::anyhow!("Command ended with status {status}"))
    } else {
        Ok(())
    }
}
