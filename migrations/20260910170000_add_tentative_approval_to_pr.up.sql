ALTER TABLE pull_request
ADD COLUMN approval_tentative BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE pull_request
ADD CONSTRAINT pull_request_approval_state_consistent CHECK (
    (
        approved_by IS NULL
        AND approved_sha IS NULL
        AND approval_tentative = FALSE
    )
    OR (
        approved_by IS NOT NULL
        AND approved_sha IS NOT NULL
    )
) NOT VALID;
