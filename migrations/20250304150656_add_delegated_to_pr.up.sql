-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN delegated BOOLEAN NOT NULL DEFAULT FALSE;