use crate::bors::RepositoryState;
use crate::config::{Ec2RunnersConfig, JitRunnerKind};
use anyhow::Context;

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

/// Starts an EC2 instance on AWS, which should run a self-hosted GitHub Actions runner
/// that will be able to execute a job with the given `label`.
pub async fn start_ec2_github_runner(
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

    let runner_name = uuid::Uuid::new_v4();
    let jit_config = match ec2.jit_runner {
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
    };
    tracing::info!(
        "Registered runner {} with name: {}",
        jit_config.runner.id,
        jit_config.runner.name
    );

    let script = LAUNCH_SCRIPT.replace("$INSTALL_RUNNER", "true");
    let script = script.replace("$JITCONFIG", &jit_config.encoded_jit_config);

    // Using the AWS cli is not ideal, but the alternative (depending on aws-config, aws-sdk-ssm and
    // asd-sdk-ec2) has a massive impact on build times and binary size, plus it currently runs into
    // feature hell (ring vs aws-lc-sys). The choice might be reevaluated in the future.
    let instance_type = label.instance_type;
    let mut ec2_cli = prepare_aws_cli();
    ec2_cli
        .arg("ec2")
        .arg("run-instances")
        .arg("--region")
        .arg(&ec2.region)
        .arg("--image-id")
        .arg(format!("resolve:ssm:{image_name}"))
        .arg("--instance-type")
        .arg(instance_type)
        .arg("--launch-template")
        .arg("LaunchTemplateName=gha-runner,Version=14")
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

fn prepare_aws_cli() -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("aws");
    cmd.kill_on_drop(true);
    cmd
}

async fn run_command(cmd: &mut tokio::process::Command) -> anyhow::Result<String> {
    let output = cmd.output().await?;
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
