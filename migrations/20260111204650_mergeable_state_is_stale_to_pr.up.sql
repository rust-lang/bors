-- Add up migration script here
ALTER TABLE pull_request
    ADD COLUMN mergeable_state_is_stale BOOLEAN NOT NULL DEFAULT true;
