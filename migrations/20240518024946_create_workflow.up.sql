CREATE TABLE IF NOT EXISTS workflow (
  id SERIAL PRIMARY KEY,
  build_id INT NOT NULL,
  name TEXT NOT NULL,
  run_id BIGINT NOT NULL,
  url TEXT NOT NULL,
  status TEXT NOT NULL,
  type TEXT NOT NULL,
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT fk_build_id FOREIGN KEY (build_id) REFERENCES build(id)
);

-- create index for build url
CREATE UNIQUE INDEX IF NOT EXISTS workflow_build_id_url_idx ON workflow (build_id, url);
