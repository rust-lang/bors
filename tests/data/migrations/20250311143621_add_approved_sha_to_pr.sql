UPDATE pull_request
SET
    approved_sha = 'abc123def456'
WHERE
    id = 1;

UPDATE pull_request
SET
    approved_sha = 'fed987cba654'
WHERE
    id = 2;
