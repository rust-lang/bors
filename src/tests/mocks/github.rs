use graphql_parser::query::{Definition, Document, OperationDefinition, Selection};
use octocrab::Octocrab;
use std::collections::HashMap;
use std::sync::Arc;
use wiremock::{MockServer, Request, ResponseTemplate};

use crate::create_github_client;
use crate::github::api::client::HideCommentReason;
use crate::tests::GitHubState;
use crate::tests::mocks::app::{AppHandler, default_app_id};
use crate::tests::mocks::dynamic_mock_req;
use crate::tests::mocks::pull_request::CommentMsg;
use crate::tests::mocks::repository::{mock_repo, mock_repo_list};

pub struct GitHubMockServer {
    mock_server: MockServer,
    github: Arc<tokio::sync::Mutex<GitHubState>>,
}

impl GitHubMockServer {
    pub async fn start(github: Arc<tokio::sync::Mutex<GitHubState>>) -> Self {
        let mock_server = MockServer::start().await;

        {
            let gh_locked = github.lock().await;
            mock_repo_list(&gh_locked, &mock_server).await;
            mock_graphql(github.clone(), &mock_server).await;

            // Repositories are mocked separately to make it easier to
            // pass comm. channels to them.
            for repo in gh_locked.repos.values() {
                mock_repo(repo.clone(), &mock_server).await;
            }
        }

        AppHandler::default().mount(&mock_server).await;

        Self {
            mock_server,
            github,
        }
    }

    pub fn client(&self) -> Octocrab {
        create_github_client(
            default_app_id().into(),
            self.mock_server.uri(),
            GITHUB_MOCK_PRIVATE_KEY.into(),
        )
        .unwrap()
    }

    /// Make sure that there are no leftover events left in the queues.
    pub async fn assert_empty_queues(self) {
        // This will remove all mocks and thus also any leftover
        // channel senders, so that we can be sure below that the `recv`
        // call will not block indefinitely.
        self.mock_server.reset().await;
        drop(self.mock_server);

        for (name, repo) in self.github.lock().await.repos.iter_mut() {
            let prs = repo
                .lock()
                .pull_requests
                .values()
                .cloned()
                .collect::<Vec<_>>();
            for pr in prs {
                // Send close message
                pr.comment_queue_tx.send(CommentMsg::Close).await.unwrap();

                // Make sure that the close message is received, and nothing before it.
                let msg = pr
                    .comment_queue_rx
                    .lock()
                    .await
                    .recv()
                    .await
                    .expect("Empty comment queue");
                match msg {
                    CommentMsg::Comment(comment) => {
                        panic!(
                            "Expected that PR {name}#{} won't have any further received comments, but it received {comment:?}",
                            pr.number
                        );
                    }
                    CommentMsg::Close => {
                        // The queue was correctly empty
                    }
                };
            }
        }
    }
}

async fn mock_graphql(github: Arc<tokio::sync::Mutex<GitHubState>>, mock_server: &MockServer) {
    dynamic_mock_req(
        move |request: &Request, []: [&str; 0]| {
            #[derive(serde::Deserialize)]
            struct GraphQlRequest {
                query: String,
                variables: serde_json::Value,
            }

            // Do some basic parsing of GraphQL. We only support the small subset of operations
            // used by bors.
            let body: GraphQlRequest = request.body_json().unwrap();
            let query: Document<&str> =
                graphql_parser::parse_query(&body.query).expect("Could not parse GraphQL query");
            let definition = query.definitions.into_iter().next().unwrap();
            let selection_set = match definition {
                Definition::Operation(OperationDefinition::Mutation(mutation)) => {
                    mutation.selection_set
                }
                Definition::Operation(OperationDefinition::Query(query)) => query.selection_set,
                _ => panic!("Unexpected GraphQL query: {}", body.query),
            };
            let selection = selection_set.items.into_iter().next().unwrap();
            let operation = match selection {
                Selection::Field(field) => field,
                _ => panic!("Unexpected GraphQL selection"),
            };

            match operation.name {
                "minimizeComment" => {
                    #[derive(serde::Deserialize)]
                    struct Variables {
                        node_id: String,
                        reason: HideCommentReason,
                    }

                    let github = github.clone();
                    let data: Variables = serde_json::from_value(body.variables).unwrap();

                    // We have to use e.g. `blocking_lock` to lock from a sync function.
                    // It has to happen in a separate thread though.
                    std::thread::spawn(move || {
                        github
                            .blocking_lock()
                            .modify_comment(&data.node_id, |c| c.hide_reason = Some(data.reason));
                    })
                    .join()
                    .unwrap();
                    ResponseTemplate::new(200).set_body_json(HashMap::<String, String>::new())
                }
                "updateIssueComment" => {
                    #[derive(serde::Deserialize)]
                    struct Variables {
                        id: String,
                        body: String,
                    }

                    let data: Variables = serde_json::from_value(body.variables).unwrap();
                    let response = serde_json::json!({
                        "issueComment": {
                            "id": data.id,
                        }
                    });

                    let github = github.clone();
                    std::thread::spawn(move || {
                        github
                            .blocking_lock()
                            .modify_comment(&data.id, |c| c.content = data.body);
                    })
                    .join()
                    .unwrap();

                    ResponseTemplate::new(200).set_body_json(response)
                }
                // Get comment content
                "node" => {
                    #[derive(serde::Deserialize)]
                    struct Variables {
                        node_id: String,
                    }

                    let data: Variables = serde_json::from_value(body.variables).unwrap();

                    let github = github.clone();
                    let comment_text = std::thread::spawn(move || {
                        github
                            .blocking_lock()
                            .get_comment_by_node_id(&data.node_id)
                            .unwrap()
                            .content
                            .clone()
                    })
                    .join()
                    .unwrap();
                    let response = serde_json::json!({
                        "data": {
                            "node": {
                                "body": comment_text
                            }
                        }
                    });
                    ResponseTemplate::new(200).set_body_json(response)
                }
                _ => panic!("Unexpected GraphQL operation {}", operation.name),
            }
        },
        "POST",
        "^/graphql$".to_string(),
    )
    .mount(mock_server)
    .await;
}

const GITHUB_MOCK_PRIVATE_KEY: &str = r###"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDa2WIpKFzkeys9
R1Mn+kCM+UVAFT47QK98iTcXIG982rXPYdL2+jtzUoUc7irfZnScIiFmO8U2O2Nm
nchI/bjdceGGxyCt04PdYVgYpUWfqp2rbiqi6V6L39j23zxGUbf8chQsviW7HuCl
B105H/DQdhuHDmhOOWBg1Hv+uepp0pF86WPiz4/Ez9uNmIsoJps2pvOIi8nf/8k2
iRClRynYbE8495rD5ZGRHRkPG4wXYoKv/iH8Q7+kO+72sBk0oqmkEoUdUz+itQOB
CnWZWBC5vXb+vopw30vRZi0FNoArzN5WH2pXUKuFYkIIBWb9Aq6TDmsjPPth/VCb
DBaSJuM7AgMBAAECggEAOFkERyiXUlTMO0jkBkUO3b1IsUlG7qanCF+kCZZWXkVJ
zo2XbfPb3sN+doZ0D3UnzROUmegFzQLZgxBZA0IgmRO7R6J5rYfqSdPIhP/4vzWE
xyDkZXHE4CrQiC/OKyTbRGpy+1oyCM3YdWVCAXVR4bqnN8zj2lA3mnbbPijMTFZr
B67VdKF16qp9L2A8cEKdEHs+m86l/y6ySSpHHOLFCZblUDjeb2dqTg7DWod1Ughj
kj8FbyeNQovn0KMH3qkdYOAxC+paJaKYdd680ewKO1i+zbI6bauoUUZsLzKwdh5M
FrQhVpZ9Gc0Rroyyp85WMCCy1eyyHo732RuN0qyicQKBgQDg9+5p4XVVlcObjnHi
adfviAYxNbXu1T3S8Yejvva8c75ImHeo9pt0X4yUsnLuqflmM8UbzBbn8+PbXfux
rXiZPxl7wR7kDkKpmTwsTFOxYB2CgC4qetlcylG1kGSgckbz2dYYMmmK6ZauMBlU
dHfzaGJ8aUC2R/ZLl0ZxQVNNNQKBgQD5CV3vybh+RUCfm6qm6lGAs1ONFjjliwkQ
CE/bTGspBq37WlhWaeO+I3BkqivozlQ92jcGwxQq8oDhvEieucqDyYjUvp1Pxt1e
LT3gLy1kq1pREBI1bB1zCK1HpGn8eigIcuB8nOE+4tMUeKQNH++mhta7MgcSOoH9
4hIQgzAsrwKBgDnS4D/syGjoJq/8C/+jLvKNZvINGSc7PjnTBQcslWTY5ybnsZIH
WOuvh4XM3EfF/qmrUtWTPqv9/yoqXQBNUzsogddSSytZEv9euJ22PKjRyKP7aGJY
0zfLdPcTFxo6ZUxWSHZNtt0Srz00db5EdXRl9zJ9JznzAzZoup1vqgalAoGANRmw
M+7ZLeNqUh4JFyojUsPp7s1sOFWbCxYaoPH8b3UDJ/Mtns9ZRjOcRXqbfjpwb/fV
f9WcuUOYA4n4GhAXhF42lNZICLiofuo6pVCp5ys6SMqad1WkOeEBwaLnDnSlkJee
EjQJOzV2OIk4waurl+BsbOHP7C0Zhp7rpyWx4fUCgYEAp/4UceUfbJZGa8CcWZ8F
7M0LU9Q+tGPkP2W87zkzP82PF0bQCPT3dBP0ZduDchacrF0bqEcvMLWKwwUSNvkx
3VafpHjFw+fMUjcIkQk0VfdbRD5fLDQpJy6hUVq6A+duSqTvlhE8DFAdBAC3VZ9k
34PVnCZP7HB3k2eBSpDp4vk=
-----END PRIVATE KEY-----"###;
