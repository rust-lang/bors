-- Add up migration script here
ALTER TABLE pull_request
    ADD COLUMN head_branch TEXT NOT NULL DEFAULT 'unknown';
