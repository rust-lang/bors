-- Add up migration script here
CREATE TABLE repository (
    id SERIAL PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    treeclosed INT NULL,
    treeclosed_src TEXT,
    created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()
);