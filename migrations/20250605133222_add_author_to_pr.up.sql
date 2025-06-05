-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN author TEXT NOT NULL DEFAULT '';
