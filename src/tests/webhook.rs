use axum::body::Body;
use hmac::{Hmac, Mac};
use http::Request;
use serde::Serialize;
use sha2::Sha256;

pub const TEST_WEBHOOK_SECRET: &str = "ABCDEF";

pub fn create_webhook_request<S: Serialize>(event: &str, content: S) -> Request<Body> {
    let body = serde_json::to_string(&content).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(TEST_WEBHOOK_SECRET.as_bytes()).unwrap();
    mac.update(body.as_bytes());
    let signature = hex::encode(mac.finalize().into_bytes());

    let signature = format!("sha256={signature}");

    Request::post("/github")
        .header("x-github-event", event)
        .header("x-hub-signature-256", signature)
        .header("Content-Type", "application/json")
        .body(Body::from(body))
        .unwrap()
}
