use axum::async_trait;

pub struct PullRequest {
    number: u32,
}

/// Provides functionality for working with a remote repository.
#[async_trait]
trait RepositoryAccess {
    async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()>;
}

pub struct BorsService;

impl BorsService {
    async fn ping(&self, repo: &dyn RepositoryAccess, pr: PullRequest) -> anyhow::Result<()> {
        log::debug!("Executing ping");
        repo.post_comment(&pr, "Pong 🏓!").await?;
        Ok(())
    }

    /*async fn try_merge(
            &self,
            repo: &RepositoryClient,
            author: &GithubUser,
            pr: PullRequest,
        ) -> anyhow::Result<()> {
            log::debug!("Executing try on {}/{}", repo.name(), pr.number);

            let branch_label = pr.head.label.unwrap_or_else(|| "<unknown>".to_string());
            let merge_msg = format!(
                "Auto merge of #{} - {}, r={}",
                pr.number, branch_label, author.username
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
        */
}

#[cfg(test)]
mod tests {
    use crate::service::{BorsService, PullRequest, RepositoryAccess};
    use axum::async_trait;
    use std::collections::HashMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct MockAccess {
        comments: Mutex<HashMap<u32, Vec<String>>>,
    }

    #[async_trait]
    impl RepositoryAccess for MockAccess {
        async fn post_comment(&self, pr: &PullRequest, text: &str) -> anyhow::Result<()> {
            self.comments
                .lock()
                .unwrap()
                .entry(pr.number)
                .or_default()
                .push(text.to_string());
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_ping() {
        let service = BorsService;
        let access = MockAccess::default();
        service
            .ping(&access, PullRequest { number: 1 })
            .await
            .unwrap();
        assert_eq!(
            access.comments.lock().unwrap().get(&1).unwrap(),
            &vec!["Pong 🏓!".to_string()]
        );
    }
}
