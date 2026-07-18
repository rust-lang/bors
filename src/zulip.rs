use crate::utils::timing::{RetryMethod, perform_retryable};
use anyhow::Context;
use http::Method;
use secrecy::{ExposeSecret, SecretString};

pub struct ZulipClient {
    client: reqwest::Client,
    url: String,
    username: String,
    token: SecretString,
}

impl ZulipClient {
    pub fn new(url: String, username: String, token: SecretString) -> anyhow::Result<Self> {
        let client = reqwest::ClientBuilder::new()
            .user_agent("bors-rust-lang")
            .build()?;
        Ok(Self {
            client,
            url,
            username,
            token,
        })
    }

    /// Sends a message to a Zulip topic.
    pub async fn send_message<'a>(
        &self,
        recipient: Recipient<'a>,
        message: &'a str,
    ) -> anyhow::Result<()> {
        #[derive(serde::Serialize)]
        struct SerializedApi<'a> {
            #[serde(rename = "type")]
            type_: &'static str,
            to: String,
            #[serde(skip_serializing_if = "Option::is_none")]
            topic: Option<&'a str>,
            content: &'a str,
        }

        #[derive(Debug, serde::Deserialize)]
        struct MessageApiResponse {
            #[serde(rename = "id")]
            _message_id: u64,
        }

        perform_retryable("send-zulip-message", RetryMethod::default(), || async {
            let response = self
                .make_request(Method::POST, "messages")
                .form(&SerializedApi {
                    type_: match recipient {
                        Recipient::Stream { .. } => "stream",
                    },
                    to: match recipient {
                        Recipient::Stream { id, .. } => id.to_string(),
                    },
                    topic: match recipient {
                        Recipient::Stream { topic, .. } => Some(topic),
                    },
                    content: message,
                })
                .send()
                .await
                .context("fail sending Zulip message")?;

            response
                .error_for_status()?
                .json::<MessageApiResponse>()
                .await?;
            anyhow::Ok(())
        })
        .await?;
        Ok(())
    }

    fn make_request(&self, method: Method, url: &str) -> reqwest::RequestBuilder {
        self.client
            .request(method, format!("{}/api/v1/{url}", self.url))
            .basic_auth(&self.username, Some(self.token.expose_secret()))
    }
}

#[derive(Copy, Clone, serde::Serialize)]
#[serde(tag = "type")]
#[serde(rename_all = "snake_case")]
pub enum Recipient<'a> {
    Stream {
        #[serde(rename = "to")]
        id: u64,
        topic: &'a str,
    },
}
