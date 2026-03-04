ALTER TABLE rollup_member
ADD COLUMN rolled_up_sha TEXT NOT NULL DEFAULT '';

-- Backfill existing rows from member PR with `approved_sha` as the `rolled_up_sha`
UPDATE rollup_member rm
SET
    rolled_up_sha = COALESCE(pr.approved_sha, '')
FROM
    pull_request pr
WHERE
    pr.id = rm.member;
