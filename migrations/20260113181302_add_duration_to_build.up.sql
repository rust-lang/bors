-- Add up migration script here
ALTER TABLE build ADD COLUMN duration INTERVAL;
