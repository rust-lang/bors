-- rolled_up_merge_sha is the SHA of the intermediate merge commit that added the PR to a rollup
-- unroll_state is the status of unrolling of the PR after its rollup has been merged
-- position is the numeric position of the member in the rollup (0 = first PR, 1 = second PR, etc.)
ALTER TABLE rollup_member
    ADD COLUMN rolled_up_merge_sha TEXT NOT NULL DEFAULT '',
    ADD COLUMN unroll_state        TEXT NULL,
    ADD COLUMN position            INT  NOT NULL DEFAULT 0;

-- For finding unreported unrolled builds
CREATE INDEX IF NOT EXISTS rollup_member_unroll_state_idx ON rollup_member (unroll_state);

-- Backfill existing rows from `rolled_up_sha`
-- This is not really correct, but it is better than leaving the column empty
UPDATE rollup_member rm
SET rolled_up_merge_sha = rolled_up_sha
WHERE rolled_up_merge_sha = ''
