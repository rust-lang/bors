CREATE TABLE IF NOT EXISTS unrolled_commit
(
    rollup INT NOT NULL,
    member INT NOT NULL,
    perf_build_id INT NULL REFERENCES build (id),

    PRIMARY KEY (rollup, member),

    CONSTRAINT fk_unrolled_commit_rollup_member
        FOREIGN KEY (rollup, member)
        REFERENCES rollup_member (rollup, member)
        ON DELETE CASCADE,

    CONSTRAINT uq_unrolled_commit_perf_build
        UNIQUE (perf_build_id)
);
