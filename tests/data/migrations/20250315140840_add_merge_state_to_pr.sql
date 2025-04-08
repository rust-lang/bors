UPDATE pull_request
SET
    mergeable_state = 'mergeable'
WHERE
    id = 1;

UPDATE pull_request
SET
    mergeable_state = 'has_conflicts'
WHERE
    id = 2;
