-- Add up migration script here
CREATE TABLE repo_model (
    id SERIAL PRIMARY KEY,
    repo TEXT NOT NULL UNIQUE,
    treeclosed TEXT NOT NULL,
    treeclosed_src TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);