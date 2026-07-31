use crate::bors::RepositoryState;
use crate::config::{Ec2RunnersConfig, JitRunnerKind};
use anyhow::Context;
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;
use std::time::Duration;

/// Script that will be executed on the launched EC2 instance.
const LAUNCH_SCRIPT: &str = include_str!("ec2-runner-script.sh");

#[derive(Debug)]
pub struct ParsedLabel<'a> {
    /// The *full* input label.
    label: &'a str,
    /// AMI image name, which is configured in the config of the repository.
    image_name: &'a str,
    /// Instance type, e.g. c8a.12xlarge.
    instance_type: &'a str,
}

impl<'a> ParsedLabel<'a> {
    /// Parses labels (`runs-on` field of a GitHub Actions job) in the form of
    /// `<prefix>-<image-name>-<instance-type>-<label>`
    ///
    /// For example:
    /// - `ec2-ami1-c8a.12xlarge-x64-linux`
    pub fn parse(label: &'a str, label_prefix: &str) -> Option<Self> {
        let full_label = label;
        let label = label.strip_prefix(label_prefix)?.trim_start_matches('-');
        let mut iter = label.split("-");
        let image_name = iter.next()?;
        let instance_type = iter.next()?;
        Some(Self {
            label: full_label,
            image_name,
            instance_type,
        })
    }
}

/// Context necessary to perform actions related to EC2.
pub struct Ec2Context {
    role_arn: String,
}

impl Ec2Context {
    pub fn new(role_arn: String) -> Self {
        Self { role_arn }
    }
}

/// Starts an EC2 instance on AWS, which should run a self-hosted GitHub Actions runner
/// that will be able to execute a job with the given `label`.
pub async fn start_ec2_github_runner(
    ec2_ctx: &Ec2Context,
    ec2: &Ec2RunnersConfig,
    repo: &RepositoryState,
    label: ParsedLabel<'_>,
) -> anyhow::Result<()> {
    tracing::info!("Trying to start EC2 runner for label {}", label.label);
    let Some(image_name) = ec2.images.get(label.image_name) else {
        return Err(anyhow::anyhow!(
            "EC2 runner image name {} not found",
            label.image_name
        ));
    };

    // Emulate a "UUID" to avoid adding dependency on the uuid crate just for this one line.
    let runner_name = format!("{:x}", rand::random::<u128>());

    // For production usage, we want the organization JIT config kind, because it can be created
    // using a GitHub App with the `Self-hosted runners` organization permission.
    // The repository JIT runner config instead requires administrator permissions for the
    // repository, which is not great.
    //
    // We allow configuring the repository JIT runner mostly just to make local testing on personal
    // repos easier.
    let jit_config = match ec2.jit_runner {
        JitRunnerKind::Organization => {
            repo.client
                .create_org_jit_runner_config(
                    repo.repository().owner(),
                    &runner_name.to_string(),
                    ec2.runner_group_id.into(),
                    vec![label.label.to_string()],
                )
                .await?
        }
        JitRunnerKind::Repository => {
            repo.client
                .create_repo_jit_runner_config(
                    repo.repository().owner(),
                    repo.repository().name(),
                    &runner_name.to_string(),
                    ec2.runner_group_id.into(),
                    vec![label.label.to_string()],
                )
                .await?
        }
    };
    tracing::info!(
        "Registered runner {} with name: {}",
        jit_config.runner.id,
        jit_config.runner.name
    );

    let script = LAUNCH_SCRIPT.replace("$JITCONFIG", &jit_config.encoded_jit_config);

    let creds = get_aws_credentials(&ec2_ctx.role_arn).await?;

    // Using the AWS cli is not ideal, but the alternative (depending on aws-config, aws-sdk-ssm and
    // asd-sdk-ec2) has a massive impact on build times and binary size, plus it currently runs into
    // feature hell (ring vs aws-lc-sys). The choice might be reevaluated in the future.
    let instance_type = label.instance_type;
    let mut ec2_cli = prepare_aws_cli(Some(&creds));
    ec2_cli
        .arg("ec2")
        .arg("run-instances")
        .arg("--region")
        .arg(&ec2.region)
        .arg("--image-id")
        .arg(format!("resolve:ssm:{image_name}"))
        .arg("--instance-type")
        .arg(instance_type)
        // Delete the instance once it stops itself
        .arg("--instance-initiated-shutdown-behavior")
        .arg("terminate")
        .arg("--launch-template")
        .arg("LaunchTemplateName=gha-runner,Version=$Latest")
        .arg("--user-data")
        .arg(script);

    let output = run_command(&mut ec2_cli)
        .await
        .context("Cannot start ec2 instance")?;
    let launched: serde_json::Value = serde_json::from_str(&output)?;
    tracing::info!(
        "Launched {instance_type}: {}",
        launched["Instances"][0]["InstanceId"]
            .as_str()
            .unwrap_or("unknown instance id")
    );

    Ok(())
}

pub async fn terminate_old_ec2_instances(
    ec2_ctx: &Ec2Context,
    repo: Arc<RepositoryState>,
) -> anyhow::Result<()> {
    if repo.config.load().ec2_runners.is_none() {
        return Ok(());
    };

    let timeout = repo.config.load().timeout;

    #[derive(serde::Deserialize, Debug)]
    struct InstanceState {
        #[serde(rename = "Name")]
        name: String,
    }

    #[derive(serde::Deserialize, Debug)]
    struct Instance {
        #[serde(rename = "InstanceId")]
        id: String,
        #[serde(rename = "LaunchTime")]
        launch_time: chrono::DateTime<Utc>,
        #[serde(rename = "State")]
        state: InstanceState,
    }

    #[derive(serde::Deserialize, Debug)]
    struct Reservation {
        #[serde(rename = "Instances")]
        instances: Vec<Instance>,
    }

    #[derive(serde::Deserialize, Debug)]
    struct Instances {
        #[serde(rename = "Reservations")]
        reservations: Vec<Reservation>,
    }

    let creds = get_aws_credentials(&ec2_ctx.role_arn).await?;
    let instances = run_command(
        prepare_aws_cli(Some(&creds))
            .arg("ec2")
            .arg("describe-instances"),
    )
    .await
    .context("Cannot list running EC2 instances")?;
    let instances: Vec<Instance> = serde_json::from_str::<Instances>(&instances)?
        .reservations
        .into_iter()
        .flat_map(|r| r.instances)
        .collect();
    tracing::debug!(
        "Found the following EC2 instances ({}): {instances:?}",
        instances.len()
    );

    let deadline = Utc::now() - timeout;
    let too_old_ids = instances
        .into_iter()
        .filter(|instance| instance.launch_time < deadline)
        .filter(|instance| instance.state.name != "terminated")
        .map(|instance| instance.id)
        .collect::<Vec<String>>();

    if !too_old_ids.is_empty() {
        let too_old_ids = too_old_ids.join(",");
        tracing::info!("Cancelling EC2 instance(s) {too_old_ids}");

        run_command(
            prepare_aws_cli(Some(&creds))
                .arg("ec2")
                .arg("terminate-instances")
                // No need for graceful shutdown, we just want to terminate the instances
                // as fast as possible.
                .arg("--force")
                .arg("--skip-os-shutdown")
                .arg("--instance-ids")
                .arg(too_old_ids),
        )
        .await
        .context("Cannot terminate EC2 instances")?;
    }

    Ok(())
}

fn prepare_aws_cli(creds: Option<&RoleCredentials>) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("aws");
    if let Some(creds) = creds {
        cmd.env("AWS_ACCESS_KEY_ID", &creds.access_key_id);
        cmd.env("AWS_SECRET_ACCESS_KEY", &creds.secret_access_key);
        cmd.env("AWS_SESSION_TOKEN", &creds.session_token);
    }
    cmd.kill_on_drop(true);
    cmd
}

#[derive(Deserialize)]
struct RoleCredentials {
    #[serde(rename = "AccessKeyId")]
    access_key_id: String,
    #[serde(rename = "SecretAccessKey")]
    secret_access_key: String,
    #[serde(rename = "SessionToken")]
    session_token: String,
}

/// Assume the given role with the current AWS credentials, to create credentials for the role.
async fn get_aws_credentials(role_arn: &str) -> anyhow::Result<RoleCredentials> {
    #[derive(Deserialize)]
    struct Root {
        #[serde(rename = "Credentials")]
        pub credentials: RoleCredentials,
    }

    let mut cli = prepare_aws_cli(None);
    cli.arg("sts")
        .arg("assume-role")
        .arg("--role-arn")
        .arg(role_arn)
        .arg("--role-session-name")
        .arg("bors");
    let output = run_command(&mut cli)
        .await
        .context("Cannot assume role for AWS")?;
    let creds: Root =
        serde_json::from_str(&output).context("Cannot deserialize aws sts assume-role output")?;
    Ok(creds.credentials)
}

async fn run_command(cmd: &mut tokio::process::Command) -> anyhow::Result<String> {
    let output = match tokio::time::timeout(Duration::from_secs(60), cmd.output()).await {
        Ok(output) => output?,
        Err(_) => {
            return Err(anyhow::anyhow!(
                "Command {cmd:?} has timeouted after one minute"
            ));
        }
    };
    if !output.status.success() {
        Err(anyhow::anyhow!(
            "Command {cmd:?} ended with status {}.\nStdout:\n{}\n\nStderr:\n{}\n",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        ))
    } else {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::ParsedLabel;

    #[test]
    fn parse_label() {
        let label = ParsedLabel::parse("ec2-ami1-c8a.12xlarge-x64-linux", "ec2").unwrap();
        insta::assert_debug_snapshot!(label, @r#"
        ParsedLabel {
            label: "ec2-ami1-c8a.12xlarge-x64-linux",
            image_name: "ami1",
            instance_type: "c8a.12xlarge",
        }
        "#);
    }

    #[test]
    fn parse_label_different_prefix() {
        assert!(ParsedLabel::parse("x64-linux", "ec2").is_none());
    }
}
