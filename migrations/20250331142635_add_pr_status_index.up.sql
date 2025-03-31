-- Add up migration script here
CREATE INDEX IF NOT EXISTS pull_request_status_idx ON pull_request (status);