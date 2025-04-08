INSERT INTO
    workflow (build_id, name, run_id, url, status, type)
VALUES
    (
        1,
        'Test / Test',
        4937812456,
        'https://github.com/rust-lang/bors/actions/runs/4937812456',
        'success',
        'github'
    ),
    (
        1,
        'Test / Test Docker',
        4937812457,
        'https://github.com/rust-lang/bors/actions/runs/4937812457',
        'success',
        'github'
    ),
    (
        2,
        'CI / Calculate job matrix',
        4938210157,
        'https://github.com/rust-lang/cargo/actions/runs/4938210157',
        'success',
        'github'
    ),
    (
        3,
        'CI / PR - mingw-check',
        4939843621,
        'https://github.com/rust-lang/rust/actions/runs/4939843621',
        'success',
        'github'
    ),
    (
        3,
        'CI / PR - x86_64-gnu-llvm-19',
        4939843622,
        'https://github.com/rust-lang/rust/actions/runs/4939843622',
        'pending',
        'github'
    ),
    (
        3,
        'CI / PR - x86_64-gnu-tools',
        4939843623,
        'https://github.com/rust-lang/rust/actions/runs/4939843623',
        'success',
        'github'
    ),
    (
        3,
        'bors: buildbot',
        2493574,
        'https://buildbot.rust-lang.org/builders/try/builds/2493574',
        'pending',
        'external'
    );
