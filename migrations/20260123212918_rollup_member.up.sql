-- Add up migration script here
CREATE TABLE IF NOT EXISTS rollup_member
(
    -- The rollup PR
    rollup INT NOT NULL,
    -- PR that is a member of that rollup
    member INT NOT NULL,

    CONSTRAINT fk_rollup_id FOREIGN KEY (rollup) REFERENCES pull_request (id),
    CONSTRAINT fk_member_id FOREIGN KEY (member) REFERENCES pull_request (id),

    PRIMARY KEY (rollup, member)
);
