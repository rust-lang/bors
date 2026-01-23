INSERT INTO pull_request (repository, number, base_branch, status)
VALUES ('rust-lang/rust', 1, 'main', 'open'),
       ('rust-lang/rust', 2, 'main', 'open'),
       ('rust-lang/rust', 3, 'main', 'open'),
       ('rust-lang/rust', 4, 'main', 'open');

INSERT INTO rollup_member (rollup, member)
VALUES (5,
        6),
       (5,
        7),
       (6,
        8);
