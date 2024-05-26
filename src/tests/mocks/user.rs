use serde::Serialize;
use url::Url;

#[derive(Clone, Serialize)]
pub(crate) struct User {
    pub(crate) login: String,
    id: u64,
    node_id: String,
    avatar_url: Url,
    gravatar_id: String,
    url: Url,
    html_url: Url,
    followers_url: Url,
    following_url: Url,
    gists_url: Url,
    starred_url: Url,
    subscriptions_url: Url,
    organizations_url: Url,
    repos_url: Url,
    events_url: Url,
    received_events_url: Url,
    r#type: String,
    site_admin: bool,
}

pub(crate) fn default_user() -> User {
    User {
        id: 4539057,
        login: "Kobzol".to_string(),
        node_id: "MDQ6VXNlcjQ1MzkwNTc=".to_string(),
        avatar_url: "https://avatars.githubusercontent.com/u/4539057?v=4"
            .parse()
            .unwrap(),
        gravatar_id: "".to_string(),
        url: "https://api.github.com/users/Kobzol".parse().unwrap(),
        html_url: "https://github.com/Kobzol".parse().unwrap(),
        followers_url: "https://api.github.com/users/Kobzol/followers"
            .parse()
            .unwrap(),
        following_url: "https://api.github.com/users/Kobzol/following{/other_user}"
            .parse()
            .unwrap(),
        gists_url: "https://api.github.com/users/Kobzol/gists{/gist_id}"
            .parse()
            .unwrap(),
        starred_url: "https://api.github.com/users/Kobzol/starred{/owner}{/repo}"
            .parse()
            .unwrap(),
        subscriptions_url: "https://api.github.com/users/Kobzol/subscriptions"
            .parse()
            .unwrap(),
        organizations_url: "https://api.github.com/users/Kobzol/orgs".parse().unwrap(),
        repos_url: "https://api.github.com/users/Kobzol/repos".parse().unwrap(),
        events_url: "https://api.github.com/users/Kobzol/events{/privacy}"
            .parse()
            .unwrap(),
        received_events_url: "https://api.github.com/users/Kobzol/received_events"
            .parse()
            .unwrap(),
        r#type: "User".to_string(),
        site_admin: false,
    }
}
