-- Add up migration script here
ALTER TABLE repository
    ADD COLUMN treeclosed_reason TEXT NULL;
