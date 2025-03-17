-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN mergeable_state TEXT NOT NULL DEFAULT 'unknown';