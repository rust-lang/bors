use crate::tests::GitHub;
use crate::tests::mock::{GitHubUser, oauth_user_from_request};
use std::collections::HashMap;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

pub async fn mock_oauth(github: &GitHub, mock_server: &MockServer) {
    let config = github.oauth_config.clone();

    Mock::given(method("POST"))
        .and(path("/login/oauth/access_token"))
        .respond_with(move |req: &Request| {
            let mut data: HashMap<String, String> = url::form_urlencoded::parse(&req.body)
                .into_owned()
                .collect();

            if data.remove("client_id").expect("Missing client_id") != config.client_id() {
                return ResponseTemplate::new(400);
            }
            if data.remove("client_secret").expect("Missing client_secret")
                != config.client_secret()
            {
                return ResponseTemplate::new(400);
            }
            let code = data.remove("code").expect("Missing code");

            let access_token = format!("{code}-access-token");

            let mut serializer = url::form_urlencoded::Serializer::new(String::new());
            serializer.append_pair("access_token", &access_token);
            let response = serializer.finish();

            ResponseTemplate::new(200)
                .set_body_string(response)
                .append_header("Content-Type", "application/x-www-form-urlencoded")
        })
        .mount(mock_server)
        .await;

    Mock::given(method("GET"))
        .and(path("/user"))
        .respond_with(move |req: &Request| {
            let user = oauth_user_from_request(req);
            ResponseTemplate::new(200).set_body_json(GitHubUser::from(user))
        })
        .mount(mock_server)
        .await;
}
