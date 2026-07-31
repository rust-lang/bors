use crate::github::GithubRepoName;
use octocrab::models::{JobId, RunId};
use std::collections::HashMap;
use std::sync::RwLock;

/// This struct stores an in-memory cache of job status (started/completed) and some metadata
/// (job name) per repository and workflow.
///
/// It serves for displaying additional information about workflows on the queue page.
/// We store this state in a best-effort manner in-memory, without going to the database, to keep
/// it simple, because this data is not required for bors to function properly at the moment.
#[derive(Default)]
pub struct JobCache {
    workflows: RwLock<HashMap<WorkflowKey, HashMap<JobId, WorkflowJobData>>>,
}

impl JobCache {
    pub fn job_started(&self, repo: &GithubRepoName, run_id: RunId, job_id: JobId, name: &str) {
        let key = WorkflowKey {
            repo: repo.clone(),
            workflow_run_id: run_id,
        };
        let mut workflows = self.workflows.write().unwrap();
        workflows
            .entry(key)
            .or_default()
            .entry(job_id)
            // Do not overwrite the data if we got a completed event before a started event, just in
            // case
            .or_insert_with(|| WorkflowJobData {
                name: name.to_string(),
                status: WorkflowJobStatus::Started,
            });
    }

    pub fn job_completed(&self, repo: &GithubRepoName, run_id: RunId, job_id: JobId, name: &str) {
        let key = WorkflowKey {
            repo: repo.clone(),
            workflow_run_id: run_id,
        };
        let mut workflows = self.workflows.write().unwrap();
        let job = workflows
            .entry(key)
            .or_default()
            .entry(job_id)
            // This only executes in case we got a completed event before a started event, or if
            // we missed the started event
            .or_insert_with(|| WorkflowJobData {
                name: name.to_string(),
                status: WorkflowJobStatus::Completed,
            });
        job.status = WorkflowJobStatus::Completed;
    }

    pub fn get_jobs(&self, repo: &GithubRepoName, run_id: RunId) -> Vec<WorkflowJobData> {
        let key = WorkflowKey {
            repo: repo.clone(),
            workflow_run_id: run_id,
        };
        let workflows = self.workflows.read().unwrap();
        let Some(jobs) = workflows.get(&key) else {
            return Vec::new();
        };
        jobs.values().cloned().collect()
    }
}

#[derive(Hash, PartialEq, Eq)]
struct WorkflowKey {
    repo: GithubRepoName,
    workflow_run_id: RunId,
}

#[derive(Clone)]
pub enum WorkflowJobStatus {
    Started,
    Completed,
}

impl WorkflowJobStatus {
    pub fn is_completed(&self) -> bool {
        match self {
            WorkflowJobStatus::Started => false,
            WorkflowJobStatus::Completed => true,
        }
    }
}

#[derive(Clone)]
pub struct WorkflowJobData {
    pub name: String,
    pub status: WorkflowJobStatus,
}
