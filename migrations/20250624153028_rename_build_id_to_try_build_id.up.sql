-- Add up migration script here
ALTER TABLE pull_request DROP CONSTRAINT fk_build_id;

ALTER TABLE pull_request RENAME COLUMN build_id TO try_build_id;

ALTER TABLE pull_request ADD CONSTRAINT fk_try_build_id FOREIGN KEY (try_build_id) REFERENCES build(id);
