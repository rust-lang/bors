use crate::github::{GithubUser, PullRequest};

pub fn create_pr(number: u64) -> PullRequest {
    PullRequest {
        number,
        head_label: "".to_string(),
        head_ref: "".to_string(),
        base_ref: "".to_string(),
    }
}

pub fn create_user(username: &str) -> GithubUser {
    GithubUser {
        username: username.to_string(),
        html_url: format!("https://github.com/{username}").parse().unwrap(),
    }
}
