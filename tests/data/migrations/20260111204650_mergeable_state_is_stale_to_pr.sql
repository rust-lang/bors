UPDATE pull_request
SET mergeable_state_is_stale = true
WHERE id = 1;

UPDATE pull_request
SET mergeable_state_is_stale = false
WHERE id = 2;
