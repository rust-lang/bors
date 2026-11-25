interface PullRequestStats {
  total_count: number;
  in_queue_count: number;
  failed_count: number;
}

interface PendingWorkflow {
  run_id: number;
  url: string;
  completed_jobs: number;
  total_jobs: number;
  remaining_jobs: string[];
}

interface PendingBuild {
  pr_number: number;
  created_at_ms: number;
  workflow: PendingWorkflow | null;
}

type QueuePrStatus =
  | "approved"
  | "ready for merge"
  | "pending"
  | "failed"
  | "waiting for PR CI"
  | "";

type MergeableLabel = "yes" | "no" | "";

type RollupLabel = "rollup" | "always" | "iffy" | "never";

interface PullRequest {
  number: number;
  title: string;
  author: string;
  assignees: string[];
  approver: string | null;
  priority: number;
  status: QueuePrStatus;
  mergeable: MergeableLabel;
  rollup: RollupLabel | null;
  rollup_members: number[];
  is_rollupable: boolean;
  note: string | null;
}

interface TreeClosedInfo {
  priority: number;
  comment_source: string;
  reason: string | null;
}

interface Data {
  repo_name: string;
  repo_owner: string;
  repo_url: string;
  tree_closed: TreeClosedInfo | null;
  ec2_configured: boolean;
  oauth_client_id: string | null;
  stats: PullRequestStats;
  expected_remaining_duration_ms: number | null;
  average_build_duration_ms: number;
  selected_rollup_prs: number[];
  pending_builds: PendingBuild[];
  prs: PullRequest[];
}
