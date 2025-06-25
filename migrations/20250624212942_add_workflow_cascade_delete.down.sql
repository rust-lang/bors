-- Add down migration script here
ALTER TABLE workflow DROP CONSTRAINT fk_build_id;
ALTER TABLE workflow ADD CONSTRAINT fk_build_id FOREIGN KEY (build_id) REFERENCES build(id);
