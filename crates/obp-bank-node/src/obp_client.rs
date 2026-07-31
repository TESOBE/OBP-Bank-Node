//! Interface B — the client the Bank Node uses to call OBP-API.
//!
//! Submits the OPEN_CORRIDOR_PROMISE Transaction Request on the bank's settlement
//! account:
//!
//! ```text
//! POST {base_url}/obp/v7.0.0/banks/{bank_id}/accounts/{account_id}
//!      /owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests
//! ```
//!
//! The request body is the A1.1 body verbatim — the south-side and OBP-API
//! shapes are identical, so the dispatcher replays the stored payload unchanged.
//!
//! Also reports the on-chain Promise evidence back to OBP-API (the §5.1
//! salt-relay intake) once the commitment is written:
//!
//! ```text
//! POST {base_url}/obp/v7.0.0/banks/{bank_id}/accounts/{account_id}
//!      /transaction-requests/{obp_tr_id}/open-corridor/promise
//! ```
//!
//! Errors are split into two classes the dispatcher acts on differently:
//!   - [`ObpClientError::Transport`] — retryable. Unreachable, timeout, 5xx,
//!     429, *and* operational 4xx (401/403 auth, 404/405 wrong endpoint, or any
//!     4xx without an OBP business code). Leave the outbox row and back off.
//!   - [`ObpClientError::Rejected`] — terminal. A 400/422 carrying an OBP-NNNNN
//!     business code (e.g. an unroutable destination). Move the row to
//!     `EXCEPTION`. A misconfiguration must not be mistaken for this.

use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::Mutex;
use tracing::debug;

/// OBP API version segment for the Transaction Request endpoint.
const OBP_API_VERSION: &str = "v7.0.0";

#[derive(Debug, thiserror::Error)]
pub enum ObpClientError {
    /// Retryable: the call did not produce an authoritative OBP answer.
    #[error("OBP API transport error: {0}")]
    Transport(String),
    /// Terminal: OBP-API answered with a 4xx and (usually) an OBP error code.
    #[error("OBP API rejected the request ({status}): {error_code}: {message}")]
    Rejected {
        status: u16,
        error_code: String,
        message: String,
    },
}

impl ObpClientError {
    /// Whether the dispatcher should retry later rather than fail terminally.
    pub fn is_retryable(&self) -> bool {
        matches!(self, ObpClientError::Transport(_))
    }
}

/// How the Bank Node authenticates to OBP-API.
///
/// The Bank Node is a configured server-side service, so it uses OBP-API's
/// OAuth2 client-credentials (machine-to-machine) grant, or a pre-obtained
/// DirectLogin token. There is no per-request signing.
#[allow(dead_code)]
#[derive(Clone)]
pub enum ObpAuth {
    /// No credentials — local development against a mock OBP-API.
    None,
    /// OBP DirectLogin: a pre-obtained token sent as `Authorization: DirectLogin token="..."`.
    DirectLogin { token: String },
    /// OAuth2 client-credentials grant. The client exchanges `client_id` /
    /// `client_secret` at `token_url` for a short-lived bearer token (cached
    /// until shortly before expiry, then refreshed).
    ClientCredentials {
        token_url: String,
        client_id: String,
        client_secret: String,
        scope: Option<String>,
    },
}

/// A fetched OAuth2 bearer token plus the instant we should refresh it at.
#[derive(Clone)]
struct CachedToken {
    bearer: String,
    refresh_at: Instant,
}

/// The slice of the OAuth2 token-endpoint response we use.
#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    expires_in: Option<u64>,
}

pub struct ObpClient {
    http: reqwest::Client,
    base_url: String,
    auth: ObpAuth,
    /// Cached client-credentials token (unused for the other auth schemes).
    token: Mutex<Option<CachedToken>>,
}

/// The slice of the OBP Transaction Request response the Bank Node cares about:
/// the TR id assigned by OBP-API. The rest of the body is logged but unused.
#[derive(Debug, Clone)]
pub struct ObpTrAccepted {
    pub obp_transaction_request_id: Option<String>,
}

/// The report-back body — field names are the locked wire contract with
/// OBP-API (`tx_hash`, not `cardano_tx_hash`; the chain is named by
/// `blockchain`). All values are opaque strings to OBP-API: stored as
/// Transaction Request attributes and relayed verbatim to the beneficiary
/// bank inside `obp_credit_notification`.
#[derive(Debug, serde::Serialize)]
pub struct PromiseEvidence<'a> {
    pub tx_hash: &'a str,
    pub blockchain: &'a str,
    pub commitment: &'a str,
    pub salt: &'a str,
    pub preimage: &'a str,
}

impl ObpClient {
    pub fn new(base_url: impl Into<String>, auth: ObpAuth) -> Result<Self, ObpClientError> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ObpClientError::Transport(format!("building HTTP client: {e}")))?;
        Ok(Self {
            http,
            // Trim a trailing slash so URL joins are unambiguous.
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            token: Mutex::new(None),
        })
    }

    /// Attach OBP-API credentials to a request. For client-credentials this
    /// fetches (and caches) a bearer token, refreshing it shortly before expiry.
    async fn authorize(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<reqwest::RequestBuilder, ObpClientError> {
        match &self.auth {
            ObpAuth::None => Ok(req),
            ObpAuth::DirectLogin { token } => {
                Ok(req.header("Authorization", format!("DirectLogin token=\"{token}\"")))
            }
            ObpAuth::ClientCredentials { .. } => {
                let bearer = self.bearer_token().await?;
                Ok(req.header("Authorization", format!("Bearer {bearer}")))
            }
        }
    }

    /// Return a valid client-credentials bearer token, fetching a fresh one when
    /// the cache is empty or close to expiry.
    async fn bearer_token(&self) -> Result<String, ObpClientError> {
        let ObpAuth::ClientCredentials {
            token_url,
            client_id,
            client_secret,
            scope,
        } = &self.auth
        else {
            return Err(ObpClientError::Transport(
                "bearer_token called for a non-client-credentials auth scheme".into(),
            ));
        };

        let mut guard = self.token.lock().await;
        if let Some(cached) = guard.as_ref() {
            if Instant::now() < cached.refresh_at {
                return Ok(cached.bearer.clone());
            }
        }

        // Standard OAuth2 client-credentials: POST the grant to the token
        // endpoint, authenticating the client with HTTP Basic.
        let mut form = vec![("grant_type", "client_credentials".to_string())];
        if let Some(scope) = scope {
            form.push(("scope", scope.clone()));
        }
        let resp = self
            .http
            .post(token_url)
            .basic_auth(client_id, Some(client_secret))
            .form(&form)
            .send()
            .await
            .map_err(|e| ObpClientError::Transport(format!("OAuth2 token request: {e}")))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ObpClientError::Transport(format!("reading token response: {e}")))?;
        if !status.is_success() {
            return Err(ObpClientError::Transport(format!(
                "OAuth2 token endpoint returned {status}: {}",
                truncate(&text, 500)
            )));
        }

        let parsed: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| ObpClientError::Transport(format!("parsing token response: {e}")))?;

        // Refresh a little before the real expiry; default to 5 min when the
        // server omits `expires_in`.
        let lifetime = parsed.expires_in.unwrap_or(300);
        let lead = lifetime.min(30);
        let refresh_at = Instant::now() + Duration::from_secs(lifetime.saturating_sub(lead));
        *guard = Some(CachedToken {
            bearer: parsed.access_token.clone(),
            refresh_at,
        });
        Ok(parsed.access_token)
    }

    fn transaction_requests_url(&self, bank_id: &str, account_id: &str) -> String {
        format!(
            "{base}/obp/{ver}/banks/{bank}/accounts/{acct}/owner\
             /transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests",
            base = self.base_url,
            ver = OBP_API_VERSION,
            bank = bank_id,
            acct = account_id,
        )
    }

    /// Submit the OPEN_CORRIDOR_PROMISE Transaction Request. `body_json` is the raw A1.1
    /// request payload (the same JSON the south-side handler received).
    pub async fn submit_open_corridor(
        &self,
        bank_id: &str,
        account_id: &str,
        body_json: &str,
    ) -> Result<ObpTrAccepted, ObpClientError> {
        let url = self.transaction_requests_url(bank_id, account_id);
        debug!(%url, "submitting OPEN_CORRIDOR_PROMISE transaction request to OBP-API");

        let req = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .body(body_json.to_owned());
        let req = self.authorize(req).await?;

        let resp = req
            .send()
            .await
            .map_err(|e| ObpClientError::Transport(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ObpClientError::Transport(format!("reading response body: {e}")))?;

        if status.is_success() {
            let obp_transaction_request_id = extract_tr_id(&text);
            return Ok(ObpTrAccepted {
                obp_transaction_request_id,
            });
        }
        Err(classify_failure(status, &text))
    }

    fn promise_report_url(&self, bank_id: &str, account_id: &str, obp_tr_id: &str) -> String {
        format!(
            "{base}/obp/{ver}/banks/{bank}/accounts/{acct}\
             /transaction-requests/{tr}/open-corridor/promise",
            base = self.base_url,
            ver = OBP_API_VERSION,
            bank = bank_id,
            acct = account_id,
            tr = obp_tr_id,
        )
    }

    /// Report the on-chain Promise evidence back to OBP-API. `obp_tr_id` is the
    /// Transaction Request id OBP-API assigned at submit time (not the node's
    /// own id). The endpoint is idempotent: re-posting identical evidence
    /// succeeds; posting *different* evidence for a TR that already has some is
    /// refused with OBP-40053, which surfaces here as a terminal
    /// [`ObpClientError::Rejected`].
    pub async fn report_promise(
        &self,
        bank_id: &str,
        account_id: &str,
        obp_tr_id: &str,
        evidence: &PromiseEvidence<'_>,
    ) -> Result<(), ObpClientError> {
        let url = self.promise_report_url(bank_id, account_id, obp_tr_id);
        debug!(%url, "reporting Promise evidence to OBP-API");

        let req = self.http.post(&url).json(evidence);
        let req = self.authorize(req).await?;

        let resp = req
            .send()
            .await
            .map_err(|e| ObpClientError::Transport(e.to_string()))?;

        let status = resp.status();
        let text = resp
            .text()
            .await
            .map_err(|e| ObpClientError::Transport(format!("reading response body: {e}")))?;

        if status.is_success() {
            return Ok(());
        }
        Err(classify_failure(status, &text))
    }
}

/// Split a non-2xx OBP answer into the two classes the dispatcher acts on.
///
/// Only a genuine *business* rejection is terminal: a 400/422 carrying an
/// OBP-NNNNN code (e.g. an unroutable destination, or conflicting promise
/// evidence). Everything else is operational, not a verdict on the payment:
/// 5xx, timeouts (408), rate limiting (429), bad credentials (401/403), a
/// wrong/misconfigured endpoint (404/405), or a 4xx with no business code.
/// Those are retryable — failing a real payment because OBP-API was
/// misconfigured or our credentials were stale would be wrong.
fn classify_failure(status: reqwest::StatusCode, body: &str) -> ObpClientError {
    let (error_code, message) = parse_obp_error(body);
    let is_business_rejection = matches!(status.as_u16(), 400 | 422)
        && error_code.starts_with("OBP-")
        && error_code != "OBP-UNKNOWN";
    if is_business_rejection {
        ObpClientError::Rejected {
            status: status.as_u16(),
            error_code,
            message,
        }
    } else {
        ObpClientError::Transport(format!(
            "OBP-API returned {status}: {}",
            truncate(&message, 500)
        ))
    }
}

/// Pull `transaction_request_id` (or `id`) out of an OBP TR response body.
fn extract_tr_id(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v.get("transaction_request_id")
        .or_else(|| v.get("id"))
        .and_then(|x| x.as_str())
        .map(str::to_owned)
}

/// OBP error bodies look like `{"code":400,"message":"OBP-30018: ..."}`. Be
/// lenient: fall back to the raw text if the shape differs.
fn parse_obp_error(body: &str) -> (String, String) {
    #[derive(Deserialize)]
    struct ObpError {
        message: Option<String>,
    }
    if let Ok(parsed) = serde_json::from_str::<ObpError>(body) {
        if let Some(msg) = parsed.message {
            // OBP messages are typically "OBP-NNNNN: human text".
            let code = msg
                .split_once(':')
                .map(|(c, _)| c.trim())
                .filter(|c| c.starts_with("OBP-"))
                .unwrap_or("OBP-UNKNOWN")
                .to_string();
            return (code, msg);
        }
    }
    ("OBP-UNKNOWN".to_string(), truncate(body, 500))
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

    /// Spin up a throwaway OBP-API stand-in and return its base URL. The handler
    /// is supplied by the caller so each test shapes its own response.
    async fn spawn_stub(router: Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn success_extracts_transaction_request_id() {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests",
            post(|| async {
                Json(serde_json::json!({ "transaction_request_id": "obp-tr-999", "status": "INITIATED" }))
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let accepted = client
            .submit_open_corridor("ke.01.kcs", "acct-1", r#"{"value":{}}"#)
            .await
            .unwrap();
        assert_eq!(accepted.obp_transaction_request_id.as_deref(), Some("obp-tr-999"));
    }

    #[tokio::test]
    async fn client_error_is_terminal_rejection_with_obp_code() {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "code": 400, "message": "OBP-30018: Bank Account not found." })),
                )
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .submit_open_corridor("ke.01.kcs", "acct-1", "{}")
            .await
            .unwrap_err();
        assert!(!err.is_retryable(), "4xx must be terminal");
        match err {
            ObpClientError::Rejected { status, error_code, .. } => {
                assert_eq!(status, 400);
                assert_eq!(error_code, "OBP-30018");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn not_found_endpoint_is_retryable_not_terminal() {
        // A 404 (wrong/misconfigured endpoint) must not be mistaken for a
        // business rejection — otherwise a misconfig silently fails payments.
        let router = Router::new(); // no matching route → 404
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .submit_open_corridor("ke.01.kcs", "acct-1", "{}")
            .await
            .unwrap_err();
        assert!(err.is_retryable(), "404 must be operational/retryable, not terminal");
    }

    #[tokio::test]
    async fn server_error_is_retryable_transport() {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/owner/transaction-request-types/OPEN_CORRIDOR_PROMISE/transaction-requests",
            post(|| async { axum::http::StatusCode::INTERNAL_SERVER_ERROR }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .submit_open_corridor("ke.01.kcs", "acct-1", "{}")
            .await
            .unwrap_err();
        assert!(err.is_retryable(), "5xx must be retryable");
    }

    fn evidence() -> PromiseEvidence<'static> {
        PromiseEvidence {
            tx_hash: "63eacfe3dbc1",
            blockchain: "cardano",
            commitment: "9c56cc51b374",
            salt: "5f4dcc3b5aa7",
            preimage: r#"{"tx_request_id":"tr-1"}"#,
        }
    }

    #[tokio::test]
    async fn report_promise_posts_wire_body_to_obp_tr_path() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(1);
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/transaction-requests/obp-tr-9/open-corridor/promise",
            post(move |Json(body): Json<serde_json::Value>| async move {
                tx.send(body).await.unwrap();
                (axum::http::StatusCode::CREATED, Json(serde_json::json!({})))
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        client
            .report_promise("ke.01.kcs", "acct-1", "obp-tr-9", &evidence())
            .await
            .unwrap();

        let body = rx.recv().await.unwrap();
        assert_eq!(body["tx_hash"], "63eacfe3dbc1");
        assert_eq!(body["blockchain"], "cardano");
        assert_eq!(body["commitment"], "9c56cc51b374");
        assert_eq!(body["salt"], "5f4dcc3b5aa7");
        assert_eq!(body["preimage"], r#"{"tx_request_id":"tr-1"}"#);
    }

    #[tokio::test]
    async fn report_promise_evidence_conflict_is_terminal() {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/accounts/:acct/transaction-requests/:tr/open-corridor/promise",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "code": 400, "message": "OBP-40053: Open Corridor promise evidence is already attached to this Transaction Request with different values." })),
                )
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .report_promise("ke.01.kcs", "acct-1", "obp-tr-9", &evidence())
            .await
            .unwrap_err();
        assert!(!err.is_retryable(), "evidence conflict must be terminal");
        match err {
            ObpClientError::Rejected { error_code, .. } => assert_eq!(error_code, "OBP-40053"),
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn report_promise_missing_endpoint_is_retryable() {
        // Running against an OBP-API without the report-back endpoint (or a
        // misconfigured base URL) must retry, not fail the payment.
        let router = Router::new(); // no matching route → 404
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .report_promise("ke.01.kcs", "acct-1", "obp-tr-9", &evidence())
            .await
            .unwrap_err();
        assert!(err.is_retryable(), "404 must be operational/retryable");
    }

    #[tokio::test]
    async fn unreachable_host_is_retryable_transport() {
        // Port 1 is reserved and refuses connections.
        let client = ObpClient::new("http://127.0.0.1:1", ObpAuth::None).unwrap();
        let err = client
            .submit_open_corridor("ke.01.kcs", "acct-1", "{}")
            .await
            .unwrap_err();
        assert!(err.is_retryable(), "connection refused must be retryable");
    }
}
