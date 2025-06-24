-- Add up migration script here
ALTER TABLE pull_request ADD COLUMN auto_build_id INTEGER REFERENCES build(id);
