use crate::github::{GithubUser, PullRequest};
use crate::handlers::RepositoryClient;
use crate::permissions::{PermissionResolver, PermissionType};

pub async fn command_try_build<Client: RepositoryClient, Perms: PermissionResolver>(
    client: &Client,
    perms: &Perms,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<()> {
    log::debug!("Executing try on {}/{}", client.repository(), pr.number);

    if !check_try_permissions(client, perms, pr, author).await? {
        return Ok(());
    }

    Ok(())
}

async fn check_try_permissions<Client: RepositoryClient, Perms: PermissionResolver>(
    client: &Client,
    permission_resolver: &Perms,
    pr: &PullRequest,
    author: &GithubUser,
) -> anyhow::Result<bool> {
    let result = if !permission_resolver
        .has_permission(&author.username, PermissionType::Try)
        .await
    {
        client
            .post_comment(
                pr,
                &format!(
                    "@{}: :key: Insufficient privileges: not in try users",
                    author.username
                ),
            )
            .await?;
        false
    } else {
        true
    };
    Ok(result)
}

#[cfg(test)]
mod tests {
    use crate::handlers::trybuild::command_try_build;
    use crate::tests::client::test_client;
    use crate::tests::model::{create_pr, create_user};
    use crate::tests::permissions::NoPermissions;

    #[tokio::test]
    async fn test_try_no_permission() {
        let client = test_client();
        command_try_build(&client, &NoPermissions, &create_pr(1), &create_user("foo"))
            .await
            .unwrap();
        client.check_comments(
            1,
            &["@foo: :key: Insufficient privileges: not in try users"],
        );
    }
}
