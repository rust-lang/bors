-- Add up migration script here
CREATE TABLE IF NOT EXISTS repository
(
    id             SERIAL PRIMARY KEY,
    name           TEXT        NOT NULL UNIQUE,
    tree_state     INT         NULL,
    treeclosed_src TEXT,
    created_at     TIMESTAMPTZ NOT NULL DEFAULT NOW()
);
