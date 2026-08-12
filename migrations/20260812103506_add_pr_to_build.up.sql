-- Add up migration script here
ALTER TABLE build
    ADD COLUMN pr_number BIGINT NULL;

UPDATE build
SET pr_number = pr.number
FROM pull_request AS pr
WHERE pr.try_build_id = build.id
   OR pr.auto_build_id = build.id;
