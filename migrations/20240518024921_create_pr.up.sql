CREATE TABLE IF NOT EXISTS pull_request (
  id SERIAL PRIMARY KEY,
  repository TEXT NOT NULL,
  number BIGINT NOT NULL,
  build_id INT,
  created_at TIMESTAMPTZ NOT NULL DEFAULT CURRENT_TIMESTAMP,
  CONSTRAINT fk_build_id FOREIGN KEY (build_id) REFERENCES build(id)
);

-- create index for repository and number
CREATE UNIQUE INDEX IF NOT EXISTS pull_request_repository_number_idx ON pull_request (repository, number);
