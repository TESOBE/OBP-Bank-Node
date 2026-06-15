//! Interface A2 — credit delivery to the bank's core banking system (CBS).
//!
//! When an `obp_credit_notification` arrives over Interface C, the Bank Node
//! must tell the bank's CBS to credit the customer. This delivers that
//! instruction. Only the `webhook_obp` mode is implemented for now (a `POST` of
//! the OBP credit-notification JSON); the ISO-20022 / database / file-drop modes
//! in the spec are future work.
//!
//! In the PoC the CBS is `OBP_Mock_CBS`, which ACKs with `{status, cbs_reference}`.

use std::time::Duration;

use serde::Deserialize;
use tracing::debug;

#[derive(Debug, thiserror::Error)]
pub enum CbsError {
    #[error("CBS transport error: {0}")]
    Transport(String),
    #[error("CBS rejected the credit ({status}): {body}")]
    Rejected { status: u16, body: String },
}

/// The CBS's acknowledgement of a credit instruction.
#[allow(dead_code)] // `status` is captured for logging/audit; only `cbs_reference` is acted on
#[derive(Debug, Clone, Deserialize)]
pub struct CbsAck {
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub cbs_reference: Option<String>,
}

/// Delivers credit instructions to the bank's CBS. Currently the `webhook_obp`
/// mode: a bearer-authenticated `POST` of the OBP credit-notification body.
#[derive(Clone)]
pub struct CbsClient {
    http: reqwest::Client,
    url: String,
    /// Sent as `Authorization: Bearer <secret>` when set (the local secret).
    bearer: Option<String>,
}

impl CbsClient {
    pub fn new(url: impl Into<String>, bearer: Option<String>, timeout_secs: u64) -> Result<Self, CbsError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(1)))
            .build()
            .map_err(|e| CbsError::Transport(format!("building HTTP client: {e}")))?;
        Ok(Self {
            http,
            url: url.into(),
            bearer,
        })
    }

    /// Deliver a credit instruction. `body_json` is the OBP credit-notification
    /// JSON to post to the CBS. Returns the CBS acknowledgement.
    pub async fn deliver_credit(&self, body_json: &str) -> Result<CbsAck, CbsError> {
        debug!(url = %self.url, "delivering credit to CBS (webhook_obp)");
        let mut req = self
            .http
            .post(&self.url)
            .header("Content-Type", "application/json")
            .body(body_json.to_owned());
        if let Some(secret) = &self.bearer {
            req = req.header("Authorization", format!("Bearer {secret}"));
        }

        let resp = req.send().await.map_err(|e| CbsError::Transport(e.to_string()))?;
        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| CbsError::Transport(format!("reading response body: {e}")))?;

        if !status.is_success() {
            return Err(CbsError::Rejected {
                status: status.as_u16(),
                body: truncate(&text, 500),
            });
        }
        // A 2xx with an unparseable/empty body still counts as accepted.
        Ok(serde_json::from_str(&text).unwrap_or(CbsAck {
            status: Some("ACCEPTED".into()),
            cbs_reference: None,
        }))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}…", &s[..max])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{routing::post, Json, Router};
    use std::net::SocketAddr;

    async fn spawn(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn accepted_credit_returns_cbs_reference() {
        let router = Router::new().route(
            "/credit",
            post(|| async { Json(serde_json::json!({ "status": "ACCEPTED", "cbs_reference": "CBS-TXN-1" })) }),
        );
        let base = spawn(router).await;
        let client = CbsClient::new(format!("{base}/credit"), Some("secret".into()), 5).unwrap();

        let ack = client.deliver_credit(r#"{"value":{"amount":"10"}}"#).await.unwrap();
        assert_eq!(ack.status.as_deref(), Some("ACCEPTED"));
        assert_eq!(ack.cbs_reference.as_deref(), Some("CBS-TXN-1"));
    }

    #[tokio::test]
    async fn non_2xx_is_rejected() {
        let router = Router::new().route(
            "/credit",
            post(|| async { (axum::http::StatusCode::SERVICE_UNAVAILABLE, "cbs down") }),
        );
        let base = spawn(router).await;
        let client = CbsClient::new(format!("{base}/credit"), None, 5).unwrap();

        let err = client.deliver_credit("{}").await.unwrap_err();
        match err {
            CbsError::Rejected { status, .. } => assert_eq!(status, 503),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn unreachable_cbs_is_transport_error() {
        let client = CbsClient::new("http://127.0.0.1:1/credit", None, 2).unwrap();
        let err = client.deliver_credit("{}").await.unwrap_err();
        assert!(matches!(err, CbsError::Transport(_)));
    }
}
