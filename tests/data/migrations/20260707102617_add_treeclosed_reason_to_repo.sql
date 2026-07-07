UPDATE repository
SET treeclosed_reason = 'CI is flaky'
WHERE name = 'rust-lang/clippy'
