-- Add up migration script here
ALTER TABLE build ADD COLUMN check_run_id BIGINT;
