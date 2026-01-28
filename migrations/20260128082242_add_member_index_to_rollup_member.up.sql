-- Add up migration script here
CREATE INDEX IF NOT EXISTS rollup_member_member ON rollup_member (member);
