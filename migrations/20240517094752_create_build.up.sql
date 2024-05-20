BEGIN;

CREATE TABLE IF NOT EXISTS build (
  id SERIAL PRIMARY KEY,
  repository TEXT NOT NULL,
  branch TEXT NOT NULL,
  commit_sha TEXT NOT NULL,
  status TEXT NOT NULL,
  parent TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP
);

CREATE UNIQUE INDEX IF NOT EXISTS build_repository_branch_commit_sha_idx ON build (repository, branch, commit_sha);

COMMIT;
