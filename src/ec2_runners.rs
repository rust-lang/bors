use crate::bors::RepositoryState;
use crate::config::Ec2RunnersConfig;
use std::sync::Arc;

pub async fn start_ec2_runner(
    ec2: &Ec2RunnersConfig,
    repo: &RepositoryState,
    mut labels: Vec<String>,
) -> anyhow::Result<()> {
    let account = &ec2.destination_account;
    labels.extend(["x64".to_string(), "self-hosted".to_string()]);

    let runner_name = uuid::Uuid::new_v4();
    let jit_config = repo
        .client
        .client
        .actions()
        .create_org_jit_runner_config(
            repo.repository().owner(),
            &runner_name.to_string(),
            ec2.runner_group_id.into(),
            labels,
        )
        .send()
        .await?;
    tracing::info!(
        "Registered runner {} with name: {}",
        jit_config.runner.id,
        jit_config.runner.name
    );

    Ok(())
}
