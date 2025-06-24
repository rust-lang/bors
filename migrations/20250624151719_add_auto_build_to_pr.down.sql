-- Add down migration script here
ALTER TABLE pull_request DROP COLUMN auto_build_id;
