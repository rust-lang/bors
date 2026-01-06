-- Add up migration script here
CREATE TABLE IF NOT EXISTS rollup_member
(
    -- We have to store the PR through their numbers rather than a FK to the pull_request table. This is due to the rollup PR
    -- being created at the time of record insertion in this table, so we won't have an entry in pull_request for it
    rollup_pr_number  BIGINT NOT NULL,
    member_pr_number  BIGINT NOT NULL,
    repository TEXT   NOT NULL,

    PRIMARY KEY (repository, rollup_pr_number, member_pr_number)
);
