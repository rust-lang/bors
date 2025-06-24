-- The remaining should be renamed build_id -> try_build_id

UPDATE pull_request
SET
    try_build_id = 1
WHERE
    id = 1;
