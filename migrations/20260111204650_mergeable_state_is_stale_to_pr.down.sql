-- Add down migration script here
ALTER TABLE pull_request
    DROP COLUMN mergeable_state_is_stale;
