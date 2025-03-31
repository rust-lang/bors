use std::{collections::VecDeque, sync::Mutex};

use crate::github::{GithubRepoName, PullRequestNumber};

pub struct MergeableQueueItem {
    pub repository: GithubRepoName,
    pub pr_number: PullRequestNumber,
}

pub struct MergeableQueue {
    queue: Mutex<VecDeque<MergeableQueueItem>>,
}

impl MergeableQueue {
    pub fn new() -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
        }
    }

    pub fn enqueue(&self, repository: GithubRepoName, pr_number: PullRequestNumber) {
        let mut queue = self.queue.lock().unwrap();

        if !queue
            .iter()
            .any(|item| item.repository == repository && item.pr_number.0 == pr_number.0)
        {
            queue.push_back(MergeableQueueItem {
                repository,
                pr_number,
            });
        }
    }

    pub fn dequeue(&self) -> Option<MergeableQueueItem> {
        self.queue.lock().unwrap().pop_front()
    }

    pub fn is_empty(&self) -> bool {
        self.queue.lock().unwrap().is_empty()
    }
}
