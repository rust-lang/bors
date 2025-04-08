UPDATE pull_request
SET
    delegated = TRUE
WHERE
    id = 1;

UPDATE pull_request
SET
    delegated = FALSE
WHERE
    id = 2;
