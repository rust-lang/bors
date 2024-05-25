use wiremock::{
    matchers::{method, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use crate::permissions::UserPermissionsResponse;

use super::event::default_user;

pub(super) struct TeamApiMockServer {
    mock_server: MockServer,
}

impl TeamApiMockServer {
    pub(super) async fn start() -> Self {
        let mock_server = MockServer::start().await;
        let permissions =
            UserPermissionsResponse::new(vec![default_user().id].into_iter().collect());

        Mock::given(method("GET"))
            .and(path_regex(
                r"^/v1/permissions/bors\..*?\.(review|try)\.json$",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(permissions))
            .mount(&mock_server)
            .await;

        Self { mock_server }
    }

    pub(super) fn uri(&self) -> String {
        self.mock_server.uri()
    }
}
