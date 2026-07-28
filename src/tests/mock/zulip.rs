use crate::ZulipClient;
use anyhow::{Context, ensure};
use base64::Engine;
use http::{
    Method,
    header::{AUTHORIZATION, CONTENT_TYPE},
};
use std::collections::HashMap;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{header, method, path},
};

const TEST_ZULIP_USERNAME: &str = "bors-test@example.com";
const TEST_ZULIP_TOKEN: &str = "test-zulip-token";

pub(super) struct ZulipMockServer {
    mock_server: MockServer,
}

impl ZulipMockServer {
    pub(super) async fn start() -> Self {
        let mock_server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/api/v1/messages"))
            .and(header("authorization", test_authorization()))
            .and(header("content-type", "application/x-www-form-urlencoded"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "result": "success",
                "msg": "",
                "id": 1,
            })))
            .mount(&mock_server)
            .await;

        Self { mock_server }
    }

    pub(super) fn client(&self) -> ZulipClient {
        ZulipClient::new(
            self.mock_server.uri(),
            TEST_ZULIP_USERNAME.to_string(),
            TEST_ZULIP_TOKEN.into(),
        )
        .unwrap()
    }

    pub(super) async fn received_messages(&self) -> anyhow::Result<Vec<ZulipMessage>> {
        self.mock_server
            .received_requests()
            .await
            .unwrap_or_default()
            .iter()
            .map(ZulipMessage::try_from)
            .collect()
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct ZulipMessage {
    recipient: ZulipRecipient,
    content: String,
}

impl ZulipMessage {
    pub fn stream(id: u64, topic: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            recipient: ZulipRecipient::Stream {
                id,
                topic: topic.into(),
            },
            content: content.into(),
        }
    }
}

impl TryFrom<&Request> for ZulipMessage {
    type Error = anyhow::Error;

    fn try_from(request: &Request) -> Result<Self, Self::Error> {
        ensure!(
            request.method == Method::POST,
            "Expected a Zulip POST request, got {}",
            request.method
        );

        ensure!(
            request.url.path() == "/api/v1/messages",
            "Expected a Zulip messages request, got {}",
            request.url.path()
        );

        let authorization = request
            .headers
            .get(AUTHORIZATION)
            .context("Missing Zulip authorization header")?
            .to_str()
            .context("Invalid Zulip authorization header")?;

        ensure!(
            authorization == test_authorization(),
            "Unexpected Zulip authorization header"
        );

        let content_type = request
            .headers
            .get(CONTENT_TYPE)
            .context("Missing Zulip content type")?
            .to_str()
            .context("Invalid Zulip content type")?;

        ensure!(
            content_type == "application/x-www-form-urlencoded",
            "Unexpected Zulip content type `{content_type}`"
        );

        let mut form: HashMap<String, String> = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .collect();

        let message_type = take_form_field(&mut form, "type")?;

        ensure!(
            message_type == "stream",
            "Unsupported Zulip message type `{message_type}`"
        );

        let recipient = take_form_field(&mut form, "to")?;
        let recipient = recipient
            .parse::<u64>()
            .with_context(|| format!("Invalid Zulip stream ID `{recipient}`"))?;
        let topic = take_form_field(&mut form, "topic")?;
        let content = take_form_field(&mut form, "content")?;
        ensure!(form.is_empty(), "Unexpected Zulip form fields: {form:?}");

        Ok(Self::stream(recipient, topic, content))
    }
}

#[derive(Debug, Eq, PartialEq)]
enum ZulipRecipient {
    Stream { id: u64, topic: String },
}

fn take_form_field(form: &mut HashMap<String, String>, field: &str) -> anyhow::Result<String> {
    form.remove(field)
        .with_context(|| format!("Missing Zulip form field `{field}`"))
}

fn test_authorization() -> String {
    format!(
        "Basic {}",
        base64::prelude::BASE64_STANDARD
            .encode(format!("{TEST_ZULIP_USERNAME}:{TEST_ZULIP_TOKEN}"))
    )
}
