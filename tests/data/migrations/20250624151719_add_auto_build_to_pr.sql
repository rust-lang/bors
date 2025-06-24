UPDATE pull_request
SET
    auto_build_id = 2
WHERE
    id = 1;

UPDATE pull_request
SET
    auto_build_id = 3
WHERE
    id = 2;

UPDATE pull_request
SET
    auto_build_id = 1
WHERE
    id = 3;
