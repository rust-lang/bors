-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN title TEXT NOT NULL DEFAULT '';