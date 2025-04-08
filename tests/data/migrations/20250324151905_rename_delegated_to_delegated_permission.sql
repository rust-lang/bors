UPDATE pull_request
SET
    delegated_permission = 'review'
WHERE
    id = 1;

UPDATE pull_request
SET
    delegated_permission = NULL
WHERE
    id = 2;
