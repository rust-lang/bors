-- Add up migration script here
ALTER TABLE pull_request
ADD COLUMN delegated_permission TEXT;

UPDATE pull_request
SET
    delegated_permission = CASE
        WHEN delegated = TRUE THEN 'review'
        ELSE NULL
    END;

ALTER TABLE pull_request
DROP COLUMN delegated;
