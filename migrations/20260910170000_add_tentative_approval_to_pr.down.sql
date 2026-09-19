-- Clear tentative approvals before removing the marker, since the previous schema would
-- otherwise treat them as full approvals and could enqueue them for merging.
UPDATE pull_request
SET approved_by = NULL,
    approved_sha = NULL,
    auto_build_id = NULL,
    approval_tentative = FALSE
WHERE approval_tentative = TRUE;

ALTER TABLE pull_request
DROP CONSTRAINT pull_request_approval_state_consistent;

ALTER TABLE pull_request
DROP COLUMN approval_tentative;
