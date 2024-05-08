use std::collections::HashSet;

use wiremock::{
    matchers::{method, path_regex},
    Mock, MockServer, ResponseTemplate,
};

use crate::permissions::UserPermissionsResponse;

use super::repository::Repository;

pub(super) async fn setup_team_api_mock(mock_server: &MockServer) {
    let permissions = UserPermissionsResponse {
        github_ids: HashSet::new(),
    };
    let repository = Repository::default();
    Mock::given(method("GET"))
        // regex that match the path "^/v1/permissions/bors.{normalized_name}.{permission}.json$"
        .and(path_regex(format!(
            r"^/v1/permissions/bors.{}.(review|try)\.json$",
            repository.name
        )))
        .respond_with(ResponseTemplate::new(200).set_body_json(permissions))
        .mount(&mock_server)
        .await;
}
