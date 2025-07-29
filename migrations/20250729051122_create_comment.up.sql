CREATE TABLE IF NOT EXISTS comment (
  id SERIAL PRIMARY KEY,
  repository TEXT NOT NULL,
  pr_number BIGINT NOT NULL,
  tag TEXT NOT NULL,
  node_id TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE INDEX comment_repo_pr_tag_idx ON comment (repository, pr_number, tag);
