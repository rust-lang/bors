UPDATE pull_request
SET
    assignees = ARRAY['kobzol', 'ehuss']
WHERE
    id = 1;

UPDATE pull_request
SET
    assignees = ARRAY['sakib25800']
WHERE
    id = 2;

UPDATE pull_request
SET
    assignees = ARRAY['matthiaskrgr', 'flip1995', 'lnicola']
WHERE
    id = 3;

UPDATE pull_request
SET
    assignees = ARRAY[]::TEXT[]
WHERE
    id = 4;
