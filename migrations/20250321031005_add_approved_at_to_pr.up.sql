-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN approved_at TIMESTAMPTZ;