INSERT INTO
    build (repository, branch, commit_sha, status, parent)
VALUES
    (
        'rust-lang/bors',
        'automation/bors/merge',
        'f2a8b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a0',
        'success',
        'a7ec24743ca724dd4b164b3a76d29d0da9573617'
    ),
    (
        'rust-lang/cargo',
        'automation/bors/merge',
        '1a2b3c4d5e6f7a8b9c0d1e2f3a4b5c6d7e8f9a1',
        'pending',
        'b3f987c12ee248ef21d37b59a40b17e93fac7c8a'
    ),
    (
        'rust-lang/rust',
        'automation/bors/merge',
        '9f8e7d6c5b4a3928374658291047382910473829',
        'failure',
        '4ee5a1bfc10bc49f30a8f527557ac4a93a2b9d66'
    );

UPDATE pull_request
SET
    auto_build_id = 4
WHERE
    id = 1;

UPDATE pull_request
SET
    auto_build_id = 5
WHERE
    id = 2;
