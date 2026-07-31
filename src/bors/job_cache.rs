use crate::github::GithubRepoName;
use octocrab::models::{JobId, RunId};
use std::cmp::Reverse;
use std::collections::HashMap;
use std::sync::RwLock;

/// Total maximum number of workflows to remember.
/// Normally, we should have at most one active auto build per repository, but we keep a buffer.
const MAX_WORKFLOWS_PER_REPO: u64 = 5;

type Cache = HashMap<WorkflowKey, HashMap<JobId, WorkflowJobData>>;

/// This struct stores an in-memory cache of job status (started/completed) and some metadata
/// (job name) per repository and workflow.
///
/// Currently only stores jobs for auto builds.
///
/// It serves for displaying additional information about workflows on the queue page.
/// We store this state in a best-effort manner in-memory, without going to the database, to keep
/// it simple, because this data is not required for bors to function properly at the moment.
pub struct AutoWorkflowJobCache {
    workflows: RwLock<Cache>,
    max_workflows_per_repo: u64,
}

impl Default for AutoWorkflowJobCache {
    fn default() -> Self {
        Self::new(MAX_WORKFLOWS_PER_REPO)
    }
}

impl AutoWorkflowJobCache {
    pub fn new(max_workflows_per_repo: u64) -> Self {
        Self {
            workflows: Default::default(),
            max_workflows_per_repo,
        }
    }

    pub fn auto_job_started(
        &self,
        repo: &GithubRepoName,
        run_id: RunId,
        job_id: JobId,
        name: &str,
    ) {
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
        prune_cache(&mut workflows, self.max_workflows_per_repo);
    }

    pub fn auto_job_completed(
        &self,
        repo: &GithubRepoName,
        run_id: RunId,
        job_id: JobId,
        name: &str,
    ) {
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
        prune_cache(&mut workflows, self.max_workflows_per_repo);
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

/// Remove old workflow entries from the cache
fn prune_cache(cache: &mut Cache, max_workflows_per_repo: u64) {
    if cache.len() <= max_workflows_per_repo as usize {
        return;
    }

    // We assume that newer workflow runs have a higher ID.
    // So we sort the workflows by ID and remove the oldest ones.
    let mut keys_per_repo: HashMap<GithubRepoName, Vec<RunId>> = HashMap::new();
    for key in cache.keys() {
        keys_per_repo
            .entry(key.repo.clone())
            .or_default()
            .push(key.workflow_run_id);
    }
    for (repo, mut runs) in keys_per_repo {
        // Sort from the newest to the oldest
        runs.sort_by_key(|k| Reverse(*k));

        // Skip `max_workflows_per_repo` newest and remove the rest
        for run in runs.iter().skip(max_workflows_per_repo as usize) {
            cache.remove(&WorkflowKey {
                repo: repo.clone(),
                workflow_run_id: *run,
            });
        }
    }
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct WorkflowKey {
    repo: GithubRepoName,
    workflow_run_id: RunId,
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub struct WorkflowJobData {
    pub name: String,
    pub status: WorkflowJobStatus,
}

#[cfg(test)]
mod tests {
    use crate::bors::job_cache::AutoWorkflowJobCache;
    use crate::github::GithubRepoName;
    use octocrab::models::{JobId, RunId};

    #[test]
    fn start_complete_job() {
        let cache = AutoWorkflowJobCache::default();
        cache.auto_job_started(&repo(), RunId(1), JobId(1), "job1");
        cache.auto_job_completed(&repo(), RunId(1), JobId(1), "job1");
        let jobs = cache.get_jobs(&repo(), RunId(1));
        insta::assert_debug_snapshot!(jobs, @r#"
        [
            WorkflowJobData {
                name: "job1",
                status: Completed,
            },
        ]
        "#);
    }

    #[test]
    fn completed_job_before_started() {
        let cache = AutoWorkflowJobCache::default();
        cache.auto_job_completed(&repo(), RunId(1), JobId(1), "job1");
        let jobs = cache.get_jobs(&repo(), RunId(1));
        insta::assert_debug_snapshot!(jobs, @r#"
        [
            WorkflowJobData {
                name: "job1",
                status: Completed,
            },
        ]
        "#);
    }

    #[test]
    fn prune_oldest_jobs() {
        let cache = AutoWorkflowJobCache::new(2);
        for id in 0..3 {
            let job_name = format!("job{id}");
            cache.auto_job_started(&repo(), RunId(id), JobId(1), &job_name);
            cache.auto_job_completed(&repo(), RunId(id), JobId(1), &job_name);
        }

        let jobs = cache.get_jobs(&repo(), RunId(0));
        assert!(jobs.is_empty());
        for id in 1..3 {
            assert!(!cache.get_jobs(&repo(), RunId(id)).is_empty());
        }
    }

    #[test]
    fn prune_oldest_jobs_per_repo() {
        let cache = AutoWorkflowJobCache::new(1);

        let repo1 = GithubRepoName::new("foo", "repo1");
        let repo2 = GithubRepoName::new("foo", "repo2");

        cache.auto_job_completed(&repo1, RunId(1), JobId(1), "job");
        cache.auto_job_completed(&repo2, RunId(2), JobId(1), "job");
        assert!(!cache.get_jobs(&repo1, RunId(1)).is_empty());
        assert!(!cache.get_jobs(&repo2, RunId(2)).is_empty());
    }

    fn repo() -> GithubRepoName {
        GithubRepoName::new("foo", "bar")
    }
}
