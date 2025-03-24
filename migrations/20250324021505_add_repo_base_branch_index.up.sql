-- Add up migration script here
CREATE INDEX IF NOT EXISTS pull_request_repository_base_branch_idx ON pull_request (repository, base_branch);