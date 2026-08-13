-- Add down migration script here
ALTER TABLE pull_request
    DROP COLUMN unrolled_build_id;
