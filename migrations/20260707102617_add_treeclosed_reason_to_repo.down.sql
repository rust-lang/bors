-- Add down migration script here
ALTER TABLE repository
    DROP COLUMN treeclosed_reason;
