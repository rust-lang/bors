-- Add down migration script here
ALTER TABLE pull_request
ADD COLUMN delegated BOOLEAN;

UPDATE pull_request
SET
    delegated = CASE
        WHEN delegated_permission = 'review' THEN TRUE
        ELSE FALSE
    END;

ALTER TABLE pull_request
DROP COLUMN delegated_permission;
