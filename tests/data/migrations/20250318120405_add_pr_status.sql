UPDATE pull_request
SET
    status = 'merged'
WHERE
    id = 1;

UPDATE pull_request
SET
    status = 'closed'
WHERE
    id = 2;
