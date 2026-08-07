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

    fn settlements_url(&self, bank_id: &str) -> String {
        format!(
            "{base}/obp/{ver}/banks/{bank}/open-corridor/settlements",
            base = self.base_url,
            ver = OBP_API_VERSION,
            bank = bank_id,
        )
    }

    /// Trigger bilateral Open Corridor settlement via OBP-API's settlement
    /// resource (`POST /banks/BANK_ID/open-corridor/settlements`). `bank_id` is
    /// this node's own bank — `CanSettleOpenCorridor` is bank-scoped and
    /// checked there. A 201 means the settlement resource was created (ledger
    /// netting done), NOT that value has moved on the rail.
    pub async fn create_settlement(
        &self,
        bank_id: &str,
        other_bank_id: &str,
        currency: &str,
    ) -> Result<ObpSettlementResponse, ObpClientError> {
        let url = self.settlements_url(bank_id);
        debug!(%url, %other_bank_id, %currency, "creating Open Corridor settlement at OBP-API");
        let body = serde_json::json!({
            "other_bank_id": other_bank_id,
            "currency": currency,
        });
        let req = self.http.post(&url).json(&body);
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
            return parse_settlement_response(&text);
        }
        Err(classify_interactive_failure(status, &text))
    }

    /// Read one settlement from OBP-API (`GET .../open-corridor/settlements/{id}`):
    /// ledger status, rail status (from the node's recorded replies), covered
    /// promises, and the outbox message states.
    pub async fn get_settlement(
        &self,
        bank_id: &str,
        settlement_id: &str,
    ) -> Result<ObpSettlementResponse, ObpClientError> {
        let url = format!("{}/{}", self.settlements_url(bank_id), settlement_id);
        debug!(%url, "reading Open Corridor settlement from OBP-API");
        let req = self.http.get(&url);
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
            return parse_settlement_response(&text);
        }
        Err(classify_interactive_failure(status, &text))
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

    /// Fetch the ACTIVE rows of OBP-API's routing-scheme registry
    /// (`GET /obp/v7.0.0/routing-schemes`), following pagination. Feeds the
    /// node's `RoutingRegistry` cache for A1.1 beneficiary-routing validation.
    pub async fn get_routing_schemes(&self) -> Result<Vec<ObpRoutingScheme>, ObpClientError> {
        const PAGE: usize = 500; // server-side maximum
        let mut all: Vec<ObpRoutingScheme> = Vec::new();
        loop {
            let url = format!(
                "{}/obp/{}/routing-schemes?status=ACTIVE&limit={}&offset={}",
                self.base_url,
                OBP_API_VERSION,
                PAGE,
                all.len()
            );
            debug!(%url, "fetching routing-scheme registry page from OBP-API");
            let req = self.http.get(&url);
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
            if !status.is_success() {
                return Err(classify_failure(status, &text));
            }
            let page: RoutingSchemesPage = serde_json::from_str(&text).map_err(|e| {
                ObpClientError::Transport(format!("unparseable routing-schemes response: {e}"))
            })?;
            let fetched = page.routing_schemes.len();
            all.extend(page.routing_schemes);
            if fetched < PAGE || all.len() as i64 >= page.pagination.total {
                return Ok(all);
            }
        }
    }
}

/// One registry row, as listed by OBP-API. Only the fields the node validates
/// with; unknown fields are ignored.
#[derive(Debug, Clone, Deserialize)]
pub struct ObpRoutingScheme {
    pub scheme: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub address_pattern: String,
    #[serde(default)]
    pub example_address: String,
}

#[derive(Debug, Deserialize)]
struct RoutingSchemesPage {
    routing_schemes: Vec<ObpRoutingScheme>,
    pagination: RoutingSchemesPagination,
}

#[derive(Debug, Deserialize)]
struct RoutingSchemesPagination {
    total: i64,
}

/// OBP-API's settlement resource, as the node consumes it: the fields the node
/// acts on (linkage stamping) parsed out, the full body kept for passthrough
/// to the south-side caller.
#[derive(Debug, Clone)]
pub struct ObpSettlementResponse {
    pub settlement_id: Option<String>,
    /// OBP TR ids of the covered promises — both directions of the pair; the
    /// node stamps whichever of them match its own outbox rows.
    pub covered_transaction_request_ids: Vec<String>,
    /// The verbatim response body.
    pub raw: serde_json::Value,
}

fn parse_settlement_response(body: &str) -> Result<ObpSettlementResponse, ObpClientError> {
    let raw: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ObpClientError::Transport(format!("parsing settlement response: {e}")))?;
    let settlement_id = raw
        .get("settlement_id")
        .and_then(|v| v.as_str())
        .map(str::to_owned);
    let covered_transaction_request_ids = raw
        .get("covered_transaction_request_ids")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default();
    Ok(ObpSettlementResponse {
        settlement_id,
        covered_transaction_request_ids,
        raw,
    })
}

/// Classification for the *interactive* settlement calls (the south-side
/// settle trigger and corridor-status proxy). Unlike [`classify_failure`],
/// any 4xx carrying an OBP business code — including 403 missing-role and
/// 404 settlement-not-found — is passed through to the caller as a
/// [`ObpClientError::Rejected`] with its original status: the caller is an
/// operator/app that needs to see the real answer, not a dispatcher that must
/// avoid failing a payment on a misconfig.
fn classify_interactive_failure(status: reqwest::StatusCode, body: &str) -> ObpClientError {
    let (error_code, message) = parse_obp_error(body);
    if status.is_client_error() && error_code.starts_with("OBP-") && error_code != "OBP-UNKNOWN" {
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
        assert_eq!(
            accepted.obp_transaction_request_id.as_deref(),
            Some("obp-tr-999")
        );
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
            ObpClientError::Rejected {
                status, error_code, ..
            } => {
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
        assert!(
            err.is_retryable(),
            "404 must be operational/retryable, not terminal"
        );
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
    async fn create_settlement_posts_body_and_parses_covered_ids() {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<serde_json::Value>(1);
        let router = Router::new().route(
            "/obp/v7.0.0/banks/rt.bank.a/open-corridor/settlements",
            post(move |Json(body): Json<serde_json::Value>| async move {
                tx.send(body).await.unwrap();
                (
                    axum::http::StatusCode::CREATED,
                    Json(serde_json::json!({
                        "settlement_id": "settle-77",
                        "debtor_bank_id": "rt.bank.a",
                        "creditor_bank_id": "rt.bank.b",
                        "currency": "KES",
                        "net_amount": "1000.00",
                        "covered_transaction_request_ids": ["obp-tr-1", "obp-tr-2"],
                    })),
                )
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let result = client
            .create_settlement("rt.bank.a", "rt.bank.b", "KES")
            .await
            .unwrap();
        assert_eq!(result.settlement_id.as_deref(), Some("settle-77"));
        assert_eq!(
            result.covered_transaction_request_ids,
            vec!["obp-tr-1", "obp-tr-2"]
        );
        assert_eq!(result.raw["net_amount"], "1000.00");

        let sent = rx.recv().await.unwrap();
        assert_eq!(sent["other_bank_id"], "rt.bank.b");
        assert_eq!(sent["currency"], "KES");
    }

    #[tokio::test]
    async fn create_settlement_missing_role_passes_403_through() {
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/open-corridor/settlements",
            post(|| async {
                (
                    axum::http::StatusCode::FORBIDDEN,
                    Json(serde_json::json!({ "code": 403, "message": "OBP-20006: User is missing one or more roles: CanSettleOpenCorridor" })),
                )
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .create_settlement("rt.bank.a", "rt.bank.b", "KES")
            .await
            .unwrap_err();
        match err {
            ObpClientError::Rejected {
                status, error_code, ..
            } => {
                assert_eq!(
                    status, 403,
                    "interactive calls pass the real status through"
                );
                assert_eq!(error_code, "OBP-20006");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn get_settlement_not_found_passes_404_through() {
        use axum::routing::get;
        let router = Router::new().route(
            "/obp/v7.0.0/banks/:bank/open-corridor/settlements/:id",
            get(|| async {
                (
                    axum::http::StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "code": 404, "message": "OBP-40058: No Open Corridor settlement with this SETTLEMENT_ID exists for this bank." })),
                )
            }),
        );
        let base = spawn_stub(router).await;
        let client = ObpClient::new(base, ObpAuth::None).unwrap();

        let err = client
            .get_settlement("rt.bank.a", "nope")
            .await
            .unwrap_err();
        match err {
            ObpClientError::Rejected {
                status, error_code, ..
            } => {
                assert_eq!(status, 404);
                assert_eq!(error_code, "OBP-40058");
            }
            other => panic!("expected Rejected, got {other:?}"),
        }
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
