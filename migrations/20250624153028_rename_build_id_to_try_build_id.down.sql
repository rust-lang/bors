-- Add down migration script here
ALTER TABLE pull_request DROP CONSTRAINT fk_try_build_id;

ALTER TABLE pull_request RENAME COLUMN try_build_id TO build_id;

ALTER TABLE pull_request ADD CONSTRAINT fk_build_id FOREIGN KEY (build_id) REFERENCES build(id);
