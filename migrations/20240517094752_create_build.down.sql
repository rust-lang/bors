BEGIN;

DROP INDEX IF EXISTS build_repository_branch_commit_sha_idx;

DROP TABLE IF EXISTS build;

COMMIT;
