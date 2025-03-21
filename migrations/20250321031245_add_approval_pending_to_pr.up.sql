-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN approval_pending BOOLEAN DEFAULT FALSE;