UPDATE pull_request
SET
    rollup = 'always'
WHERE
    id = 1;

UPDATE pull_request
SET
    rollup = 'never'
WHERE
    id = 2;
