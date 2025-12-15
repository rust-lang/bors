-- Add up migration script here
ALTER TABLE build
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'try';

UPDATE build
SET kind =
        CASE
            WHEN build.branch LIKE '%auto' THEN 'auto'
            ELSE 'try'
            END;
