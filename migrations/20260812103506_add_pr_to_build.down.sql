-- Add down migration script here
ALTER TABLE build
    DROP COLUMN pr_number;
