-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN status TEXT NOT NULL;
