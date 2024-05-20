BEGIN;

DROP INDEX IF EXISTS pull_request_repository_number_idx;

DROP TABLE IF EXISTS pull_request;

COMMIT;
