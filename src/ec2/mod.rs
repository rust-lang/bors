use crate::bors::RepositoryState;
use crate::bors::event::WorkflowJobStarted;
use crate::config::{Ec2RunnersConfig, JitRunnerKind};
use crate::database::RunId;
use crate::github::{GithubRepoName, PullRequestNumber};
use anyhow::Context;
use chrono::{DateTime, NaiveDateTime, Utc};
use octocrab::models::JobId;
use regex::Regex;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

/// Script that will be executed on the launched EC2 instance.
const LAUNCH_SCRIPT: &str = include_str!("ec2-runner-script.sh");

/// Instance tag that specifies that the given instance should be garbage collected by bors.
const TAG_BORS_TERMINATE: &str = "bors-terminate";
/// Tag with the repository for which the instance was started.
const TAG_REPO: &str = "bors-repo";
/// Tag with the workflow job ID for which the instance was started.
const TAG_JOB_ID: &str = "bors-job-id";
/// Tag with the workflow job name for which the instance was started.
const TAG_JOB_NAME: &str = "bors-job-name";
/// Tag with the workflow run ID for which the instance was started.
const TAG_RUN_ID: &str = "bors-run-id";
/// Tag with the pull request number for which the instance was started.
const TAG_PR_NUMBER: &str = "bors-pr-number";

/// How much time to wait before timeouting each aws command execution.
const AWS_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

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
    payload: &WorkflowJobStarted,
    pr_number: Option<PullRequestNumber>,
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

    let creds = get_aws_credentials(ec2_ctx).await?;

    // Hopefully a unique key that identifies this specific instance
    let instance_name = format!(
        "{}-{}-{}-{}-{}-{}",
        repo.repository().owner(),
        repo.repository().name(),
        payload.name,
        payload.run_id,
        payload.job_id,
        payload.commit_sha,
    );

    let mut tags = vec![
        ("Name", instance_name.clone()),
        (TAG_BORS_TERMINATE, "true".to_string()),
        (TAG_REPO, repo.repository().to_string()),
        (TAG_JOB_ID, payload.job_id.to_string()),
        (TAG_JOB_NAME, payload.name.clone()),
        (TAG_RUN_ID, payload.run_id.to_string()),
    ];
    if let Some(pr_number) = pr_number {
        tags.push((TAG_PR_NUMBER, pr_number.to_string()));
    }

    let tags = tags
        .into_iter()
        .map(|(k, v)| {
            format!(
                "{{Key=\"{}\",Value=\"{}\"}}",
                k.replace("\"", ""),
                v.replace("\"", "")
            )
        })
        .collect::<Vec<_>>()
        .join(",");

    // Idempotency token, to avoid starting the same instance multiple times
    // For some reason, GitHub sometimes sends us the workflow job started webhook multiple
    // times...
    let mut idempotency_token = format!("{}-{}", payload.job_id, payload.commit_sha);
    // The idempotency token cannot be longer than 64 characters
    idempotency_token.truncate(64);

    // Using the AWS cli is not ideal, but the alternative (depending on aws-config, aws-sdk-ssm and
    // asd-sdk-ec2) has a massive impact on build times and binary size, plus it currently runs into
    // feature hell (ring vs aws-lc-sys). The choice might be reevaluated in the future.
    let instance_type = label.instance_type;
    let mut ec2_cli = prepare_aws_cli(Some(&creds), Some(&ec2.region));
    ec2_cli
        .arg("ec2")
        .arg("run-instances")
        .arg("--client-token")
        .arg(&idempotency_token)
        .arg("--image-id")
        .arg(format!("resolve:ssm:{image_name}"))
        .arg("--instance-type")
        .arg(instance_type)
        // Delete the instance once it stops itself
        .arg("--instance-initiated-shutdown-behavior")
        .arg("terminate")
        .arg("--launch-template")
        .arg("LaunchTemplateName=gha-runner,Version=$Latest")
        .arg("--tag-specifications")
        .arg(format!("ResourceType=instance,Tags=[{tags}]"))
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
    let repo_config = repo.config.load();
    let Some(ec2_config) = &repo_config.ec2_runners else {
        return Ok(());
    };

    let timeout = repo_config.timeout;

    let creds = get_aws_credentials(ec2_ctx).await?;
    let instances = get_ec2_instances(repo.repository(), &creds, ec2_config)
        .await
        .context("Cannot load EC2 instances")?;
    if instances.is_empty() {
        tracing::info!("Did not find any bors EC2 instances");
        return Ok(());
    }
    tracing::info!(
        "Found the following bors EC2 instances ({}): {instances:?}",
        instances.len()
    );

    // TODO: also terminate instances whose GitHub jobs are no longer running
    let deadline = Utc::now() - timeout;
    let too_old_ids = instances
        .into_iter()
        .filter(|instance| !matches!(instance.status, Ec2InstanceStatus::Terminated))
        .filter(|instance| instance.started_at < deadline)
        .map(|instance| instance.id)
        .collect::<Vec<String>>();

    if !too_old_ids.is_empty() {
        let too_old_ids = too_old_ids.join(",");
        tracing::info!("Cancelling EC2 instance(s) {too_old_ids}");

        run_command(
            prepare_aws_cli(Some(&creds), Some(&ec2_config.region))
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
    } else {
        tracing::info!("No EC2 instances to terminate");
    }

    Ok(())
}

/// Return EC2 instances for the given repo and region that are managed by bors.
pub async fn get_ec2_instances(
    repo: &GithubRepoName,
    creds: &RoleCredentials,
    ec2_config: &Ec2RunnersConfig,
) -> anyhow::Result<Vec<Ec2Instance>> {
    #[derive(serde::Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct InstanceState {
        name: String,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Tag {
        key: String,
        value: String,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Instance {
        #[serde(rename = "InstanceId")]
        id: String,
        launch_time: chrono::DateTime<Utc>,
        state: InstanceState,
        #[serde(default)]
        tags: Vec<Tag>,
        #[serde(default, rename = "StateTransitionReason")]
        reason: Option<String>,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Reservation {
        instances: Vec<Instance>,
    }

    #[derive(serde::Deserialize, Debug)]
    #[serde(rename_all = "PascalCase")]
    struct Instances {
        reservations: Vec<Reservation>,
    }

    let mut cli = prepare_aws_cli(Some(creds), Some(&ec2_config.region));
    cli.arg("ec2").arg("describe-instances").arg("--filters");

    // Filter by bors terminate marker and repository
    cli.arg(format!("Name=tag:{TAG_BORS_TERMINATE},Values=true"));
    cli.arg(format!("Name=tag:{TAG_REPO},Values={repo}"));

    let instances = run_command(&mut cli)
        .await
        .context("Cannot list running EC2 instances")?;

    let instances = serde_json::from_str::<Instances>(&instances)?
        .reservations
        .into_iter()
        .flat_map(|r| r.instances)
        .filter_map(|instance| {
            let tags: HashMap<String, String> = instance
                .tags
                .into_iter()
                .map(|t| (t.key, t.value))
                .collect();
            let job_id = tags
                .get(TAG_JOB_ID)
                .and_then(|id| id.parse::<u64>().map(JobId).ok())?;
            let job_name = tags.get(TAG_JOB_ID).cloned()?;
            let run_id = tags
                .get(TAG_RUN_ID)
                .and_then(|id| id.parse::<u64>().map(RunId).ok())?;
            let pr_number = tags
                .get(TAG_PR_NUMBER)
                .and_then(|pr| pr.parse::<u64>().map(PullRequestNumber).ok());

            let status = match instance.state.name.as_str() {
                "pending" => Ec2InstanceStatus::Pending,
                "running" => Ec2InstanceStatus::Running,
                "shutting-down" => Ec2InstanceStatus::ShuttingDown,
                "terminated" => Ec2InstanceStatus::Terminated,
                "stopping" => Ec2InstanceStatus::Stopping,
                "stopped" => Ec2InstanceStatus::Stopped,
                _ => Ec2InstanceStatus::Unknown(instance.state.name),
            };

            let ended_at = instance.reason.and_then(|r| parse_state_transition(&r));

            Some(Ec2Instance {
                id: instance.id,
                job_id,
                job_name,
                run_id,
                started_at: instance.launch_time,
                ended_at,
                pr_number,
                status,
            })
        })
        .collect();
    Ok(instances)
}

#[derive(Clone, Debug)]
pub enum Ec2InstanceStatus {
    Pending,
    Running,
    ShuttingDown,
    Terminated,
    Stopping,
    Stopped,
    Unknown(String),
}

#[derive(Clone, Debug)]
pub struct Ec2Instance {
    pub id: String,
    pub job_id: JobId,
    pub job_name: String,
    pub run_id: RunId,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub pr_number: Option<PullRequestNumber>,
    pub status: Ec2InstanceStatus,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct RoleCredentials {
    access_key_id: String,
    secret_access_key: String,
    session_token: String,
}

/// Assume the given role with the current AWS credentials, to create credentials for the role.
pub async fn get_aws_credentials(ctx: &Ec2Context) -> anyhow::Result<RoleCredentials> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Root {
        credentials: RoleCredentials,
    }

    let mut cli = prepare_aws_cli(None, None);
    cli.arg("sts")
        .arg("assume-role")
        .arg("--role-arn")
        .arg(&ctx.role_arn)
        .arg("--role-session-name")
        .arg("bors");
    let output = run_command(&mut cli)
        .await
        .context("Cannot assume role for AWS")?;
    let creds: Root =
        serde_json::from_str(&output).context("Cannot deserialize aws sts assume-role output")?;
    Ok(creds.credentials)
}

fn prepare_aws_cli(
    creds: Option<&RoleCredentials>,
    region: Option<&str>,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("aws");
    if let Some(creds) = creds {
        cmd.env("AWS_ACCESS_KEY_ID", &creds.access_key_id);
        cmd.env("AWS_SECRET_ACCESS_KEY", &creds.secret_access_key);
        cmd.env("AWS_SESSION_TOKEN", &creds.session_token);
    }
    if let Some(region) = region {
        cmd.arg("--region").arg(region);
    }
    cmd.kill_on_drop(true);
    cmd
}

async fn run_command(cmd: &mut tokio::process::Command) -> anyhow::Result<String> {
    let output = match tokio::time::timeout(AWS_COMMAND_TIMEOUT, cmd.output()).await {
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

/// Parse State transition reason of EC2 instances, for example:
/// `User initiated (2026-08-01 18:44:49 GMT)`
fn parse_state_transition(text: &str) -> Option<DateTime<Utc>> {
    static TRANSITION_REGEX: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#"^.*\((.*?)\).*$"#).unwrap());

    let re = TRANSITION_REGEX.captures(text)?;
    let group = re.get(1)?;
    let date = NaiveDateTime::parse_from_str(group.as_str(), "%Y-%m-%d %H:%M:%S %Z").ok()?;
    Some(DateTime::from_naive_utc_and_offset(date, Utc))
}

#[cfg(test)]
mod tests {
    use super::{ParsedLabel, parse_state_transition};

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

    #[test]
    fn parse_state_transition_reason() {
        let reason = "User initiated (2026-08-01 18:44:49 GMT)";
        let date = parse_state_transition(reason).unwrap();
        insta::assert_debug_snapshot!(date, @"2026-08-01T18:44:49Z");
    }
}
