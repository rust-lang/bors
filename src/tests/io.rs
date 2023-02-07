use std::path::Path;

const ROOT_DIR: &str = env!("CARGO_MANIFEST_DIR");

pub fn load_test_file(path: &str) -> String {
    let path = Path::new(ROOT_DIR).join("tests").join("data").join(path);
    std::fs::read_to_string(path).unwrap()
}
