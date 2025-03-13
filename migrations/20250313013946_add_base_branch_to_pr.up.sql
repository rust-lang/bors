-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN base_branch TEXT NOT NULL;
