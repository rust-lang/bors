UPDATE pull_request
SET
    base_branch = 'main'
WHERE
    id = 1;

UPDATE pull_request
SET
    base_branch = 'beta'
WHERE
    id = 2;
