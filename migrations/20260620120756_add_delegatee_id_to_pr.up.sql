-- Add up migration script here
-- Note: we cannot figure GitHub author ID from the database alone.
-- So when this migration is applied, we effectively revoke all delegations,
-- so that we do not have inconsistent state, where delegatee_id would be NULL,
-- but delegated_permission would be NOT NULL.
ALTER TABLE pull_request
    ADD COLUMN delegatee_id BIGINT NULL;

UPDATE pull_request
SET delegated_permission = NULL
WHERE pull_request.delegatee_id IS NULL;
