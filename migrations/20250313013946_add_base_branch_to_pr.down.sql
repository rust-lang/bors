-- Add down migration script here
ALTER TABLE pull_request DROP COLUMN base_branch;
