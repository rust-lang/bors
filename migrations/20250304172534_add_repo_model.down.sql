-- Add down migration script here
CREATE TABLE repo_model (
    id SERIAL PRIMARY KEY,
    repo TEXT NOT NULL,
    treeclosed TEXT NOT NULL CHECK (treeclosed IN ('open', 'closed')),
    treeclosed_src TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);