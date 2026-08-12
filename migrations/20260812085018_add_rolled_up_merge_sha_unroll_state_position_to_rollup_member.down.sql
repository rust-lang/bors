ALTER TABLE rollup_member
    DROP COLUMN IF EXISTS rolled_up_merge_sha,
    DROP COLUMN IF EXISTS unroll_state,
    DROP COLUMN IF EXISTS position;
