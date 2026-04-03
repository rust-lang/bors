use crate::github::{CommitSha, GithubRepoName};
use anyhow::Context;
use secrecy::SecretString;
use std::path::{Path, PathBuf};
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

    /// Initialize a local bare repository cache if it hasn't been initialized yet.
    /// This clones the repository in blobless mode to seed the cache.
    pub async fn init_repository_cache(
        &self,
        repo_path: &Path,
        repository: &GithubRepoName,
    ) -> anyhow::Result<()> {
        if repo_path.join(".git").exists() || repo_path.join("HEAD").exists() {
            return Ok(());
        }
        if repo_path.exists() {
            std::fs::remove_dir_all(repo_path)
                .context("Cannot reset repository cache directory")?;
        }
        let repo_url = format!("https://github.com/{repository}.git");
        run_command(
            tokio::process::Command::new(&self.git)
                .kill_on_drop(true)
                .arg("clone")
                // --bare is used to avoid checking out the repository on disk, which is not needed
                .arg("--bare")
                // Treeless clone is used to avoid downloading history of all blobs and trees
                .arg("--filter=tree:0")
                .arg(&repo_url)
                .arg(repo_path),
        )
        .await
        .context("Cannot perform git clone")?;
        Ok(())
    }

    /// Prepare a local bare repository for transferring a commit.
    /// This initializes the repository (if needed) and fetches the requested commit.
    pub async fn prepare_repository_for_commit(
        &self,
        repo_path: &Path,
        source_repo: &GithubRepoName,
        commit: &CommitSha,
    ) -> anyhow::Result<()> {
        self.init_repository_cache(repo_path, source_repo).await?;

        let source_repo_url = format!("https://github.com/{source_repo}.git");

        // We reuse a cached bare repository, so we perform a regular fetch.
        tracing::debug!("Fetching commit");
        run_command(
            tokio::process::Command::new(&self.git)
                .kill_on_drop(true)
                .current_dir(repo_path)
                .arg("fetch")
                .arg(source_repo_url)
                .arg(commit.as_ref()),
        )
        .await
        .context("Cannot perform git fetch")?;
        Ok(())
    }

    /// Pushes a commit from the prepared local repository to the target repository.
    /// Note: the repository at `repo_path` must already contain `commit`.
    pub async fn transfer_commit_between_repositories(
        &self,
        repo_path: &Path,
        target_repo: &GithubRepoName,
        commit: &CommitSha,
        target_branch: &str,
        token: SecretString,
    ) -> anyhow::Result<()> {
        use secrecy::ExposeSecret;

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
                .current_dir(repo_path)
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
