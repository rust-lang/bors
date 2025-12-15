-- Add down migration script here
ALTER TABLE build
    DROP COLUMN kind;
