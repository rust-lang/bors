use crate::permissions::PermissionType;
use parking_lot::Mutex;
use rust_team_data::v1::TeamKind;
use std::sync::Arc;
use wiremock::matchers::path;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

use crate::TeamApiClient;
use crate::tests::GitHub;
use crate::tests::Repo;

pub struct TeamApiMockServer {
    mock_server: MockServer,
}

impl TeamApiMockServer {
    pub async fn start(github: Arc<Mutex<GitHub>>) -> Self {
        let mock_server = MockServer::start().await;

        let add_mock = |repo: Arc<Mutex<Repo>>, kind: PermissionType, name: &str| {
            let repo = repo.lock();
            let mut users = repo.permissions.users.iter().collect::<Vec<_>>();
            users.sort_by_key(|p| p.0.github_id);
            users.retain(|p| p.1.contains(&kind));

            let permissions = rust_team_data::v1::Permission {
                people: vec![],
                github_users: vec![],
                github_ids: users.iter().map(|(user, _)| user.github_id).collect(),
                discord_ids: vec![],
            };

            Mock::given(method("GET"))
                .and(path(format!(
                    "/v1/permissions/bors.{}.{name}.json",
                    repo.full_name().name()
                )))
                .respond_with(ResponseTemplate::new(200).set_body_json(permissions))
        };

        let repos: Vec<_> = github.lock().repos.values().cloned().collect();
        for repo in repos {
            if repo.lock().fork {
                continue;
            }

            add_mock(repo.clone(), PermissionType::Review, "review")
                .mount(&mock_server)
                .await;
            add_mock(repo.clone(), PermissionType::Try, "try")
                .mount(&mock_server)
                .await;
        }

        let people: rust_team_data::v1::People = {
            let gh = github.lock();
            rust_team_data::v1::People {
                people: gh
                    .users()
                    .into_iter()
                    .map(|user| {
                        (
                            user.name.clone(),
                            rust_team_data::v1::Person {
                                name: user.name,
                                email: None,
                                github_id: user.github_id,
                                github_sponsors: false,
                            },
                        )
                    })
                    .collect(),
            }
        };

        Mock::given(method("GET"))
            .and(path("/v1/people.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(people))
            .mount(&mock_server)
            .await;

        let teams: rust_team_data::v1::Teams = {
            let gh = github.lock();
            rust_team_data::v1::Teams {
                teams: gh
                    .teams()
                    .into_iter()
                    .map(|name| {
                        (
                            name.clone(),
                            rust_team_data::v1::Team {
                                name,
                                kind: TeamKind::Team,
                                subteam_of: None,
                                top_level: None,
                                members: vec![],
                                alumni: vec![],
                                github: None,
                                website_data: None,
                                roles: vec![],
                            },
                        )
                    })
                    .collect(),
            }
        };

        Mock::given(method("GET"))
            .and(path("/v1/teams.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(teams))
            .mount(&mock_server)
            .await;

        Self { mock_server }
    }

    pub fn client(&self) -> TeamApiClient {
        TeamApiClient::new(self.mock_server.uri())
    }
}
