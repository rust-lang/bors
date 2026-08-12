-- Add up migration script here
ALTER TABLE pull_request
    ADD COLUMN unrolled_build_id INTEGER REFERENCES build (id);
