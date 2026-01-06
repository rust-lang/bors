-- Add down migration script here
ALTER TABLE pull_request
    DROP COLUMN head_branch;
