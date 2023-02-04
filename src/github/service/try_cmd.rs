use crate::github::api::operations::merge_branches;
use crate::github::api::RepositoryClient;
use octocrab::models::pulls::PullRequest;

pub async fn try_command(repo: &RepositoryClient, pr: PullRequest) -> anyhow::Result<()> {
    log::debug!("Executing try on {}/{}", repo.name(), pr.id);

    // Just merge the PR directly right now. Let's hope that tests pass :)
    merge_branches(
        repo,
        &pr.base.ref_field,
        &pr.head.ref_field,
        "Bors merge commit",
    )
    .await
    .unwrap();

    Ok(())
}
