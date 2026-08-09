//! OIDC login for the `/setup` operator page — the same scheme the OBP
//! Portal / API Manager use (`OBP-Frontend/packages/shared/src/lib/server/oauth`):
//!
//! 1. `GET {obp}/obp/v5.1.0/well-known` lists the instance's OIDC providers
//!    as `{provider, url}` pairs; we pick the configured one (default
//!    `obp-oidc`) and fetch its discovery document for the authorization and
//!    token endpoints. A `discovery_url` config override skips the list.
//! 2. `/setup/login` redirects the admin's browser to the authorization
//!    endpoint with `state` + PKCE (S256).
//! 3. `/setup/callback` exchanges the code server-side (client id/secret +
//!    `code_verifier` in the form body — the shape OBP-OIDC accepts, which
//!    Keycloak also takes) and stores the tokens in an in-memory session
//!    keyed by an opaque HttpOnly cookie. The browser never sees a token.
//!
//! Sessions refresh once on expiry when a `refresh_token` is present and are
//! dropped otherwise — the admin just logs in again.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context};
use base64::Engine;
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::ObpApiConfig;

const SESSION_COOKIE: &str = "obp_setup_session";
/// Pending logins older than this are dropped (abandoned browser tabs).
const PENDING_TTL: Duration = Duration::from_secs(600);
/// Fallback session lifetime when the token response has no `expires_in`.
const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(3600);

#[derive(Debug, Clone, Deserialize)]
struct WellKnownList {
    well_known_uris: Vec<WellKnownUri>,
}

#[derive(Debug, Clone, Deserialize)]
struct WellKnownUri {
    provider: String,
    url: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Endpoints {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

struct PendingLogin {
    code_verifier: String,
    created: Instant,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub access_token: String,
    refresh_token: Option<String>,
    expires_at: Instant,
}

pub struct Oidc {
    cfg: ObpApiConfig,
    http: reqwest::Client,
    /// Discovered endpoints, cached after the first successful lookup so a
    /// briefly-down provider doesn't wedge the page permanently.
    endpoints: RwLock<Option<Endpoints>>,
    pending: Mutex<HashMap<String, PendingLogin>>,
    sessions: Mutex<HashMap<String, Session>>,
}

fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    hex::encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(digest)
}

impl Oidc {
    pub fn new(cfg: ObpApiConfig, http: reqwest::Client) -> Self {
        Oidc {
            cfg,
            http,
            endpoints: RwLock::new(None),
            pending: Mutex::new(HashMap::new()),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// The provider's endpoints, discovered on first use: well-known list →
    /// configured provider → discovery document (or the configured
    /// `discovery_url` directly).
    pub async fn endpoints(&self) -> anyhow::Result<Endpoints> {
        if let Some(ep) = self.endpoints.read().await.clone() {
            return Ok(ep);
        }
        let discovery_url = match &self.cfg.discovery_url {
            Some(url) => url.clone(),
            None => {
                let url = format!(
                    "{}/obp/v5.1.0/well-known",
                    self.cfg.base_url.trim_end_matches('/')
                );
                let list: WellKnownList = self
                    .http
                    .get(&url)
                    .send()
                    .await
                    .with_context(|| format!("fetching {url}"))?
                    .error_for_status()
                    .with_context(|| format!("fetching {url}"))?
                    .json()
                    .await
                    .context("parsing well-known list")?;
                let known: Vec<String> = list
                    .well_known_uris
                    .iter()
                    .map(|u| u.provider.clone())
                    .collect();
                list.well_known_uris
                    .into_iter()
                    .find(|u| u.provider == self.cfg.oauth_provider)
                    .map(|u| u.url)
                    .ok_or_else(|| {
                        anyhow!(
                            "OBP-API advertises no OIDC provider named {:?} (available: {:?})",
                            self.cfg.oauth_provider,
                            known
                        )
                    })?
            }
        };
        let ep: Endpoints = self
            .http
            .get(&discovery_url)
            .send()
            .await
            .with_context(|| format!("fetching OIDC discovery document {discovery_url}"))?
            .error_for_status()
            .with_context(|| format!("fetching OIDC discovery document {discovery_url}"))?
            .json()
            .await
            .context("parsing OIDC discovery document")?;
        info!(
            provider = %self.cfg.oauth_provider,
            authorization_endpoint = %ep.authorization_endpoint,
            "OIDC endpoints discovered"
        );
        *self.endpoints.write().await = Some(ep.clone());
        Ok(ep)
    }

    /// Mint state + PKCE for a new login and return the authorization URL to
    /// redirect the browser to.
    pub async fn begin_login(&self) -> anyhow::Result<String> {
        let ep = self.endpoints().await?;
        let state = random_token();
        let code_verifier = random_token();
        let challenge = pkce_challenge(&code_verifier);
        {
            let mut pending = self.pending.lock().unwrap();
            pending.retain(|_, p| p.created.elapsed() < PENDING_TTL);
            pending.insert(
                state.clone(),
                PendingLogin {
                    code_verifier,
                    created: Instant::now(),
                },
            );
        }
        let mut url = reqwest::Url::parse(&ep.authorization_endpoint)
            .context("authorization_endpoint is not a valid URL")?;
        url.query_pairs_mut()
            .append_pair("response_type", "code")
            .append_pair("client_id", &self.cfg.oauth_client_id)
            .append_pair("redirect_uri", &self.cfg.callback_url)
            .append_pair("scope", "openid")
            .append_pair("state", &state)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256");
        Ok(url.to_string())
    }

    /// Exchange the callback's code for tokens and open a session. Returns
    /// the session id to set as the cookie value.
    pub async fn complete_login(&self, state: &str, code: &str) -> anyhow::Result<String> {
        let Some(pending) = self.pending.lock().unwrap().remove(state) else {
            return Err(anyhow!("unknown or expired login state"));
        };
        if pending.created.elapsed() >= PENDING_TTL {
            return Err(anyhow!("unknown or expired login state"));
        }
        let ep = self.endpoints().await?;
        let tokens = self
            .token_request(&ep, &[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", &self.cfg.callback_url),
                ("client_id", &self.cfg.oauth_client_id),
                ("client_secret", &self.cfg.oauth_client_secret),
                ("code_verifier", &pending.code_verifier),
            ])
            .await?;
        let session_id = random_token();
        self.sessions.lock().unwrap().insert(
            session_id.clone(),
            Session {
                expires_at: expiry(&tokens),
                access_token: tokens.access_token,
                refresh_token: tokens.refresh_token,
            },
        );
        Ok(session_id)
    }

    /// The session's access token, refreshed once if expired. `None` means
    /// the caller must log in again.
    pub async fn access_token(&self, session_id: &str) -> Option<String> {
        let session = self.sessions.lock().unwrap().get(session_id).cloned()?;
        if session.expires_at > Instant::now() {
            return Some(session.access_token);
        }
        let refresh_token = session.refresh_token.clone()?;
        let ep = self.endpoints().await.ok()?;
        match self
            .token_request(&ep, &[
                ("grant_type", "refresh_token"),
                ("refresh_token", &refresh_token),
                ("client_id", &self.cfg.oauth_client_id),
                ("client_secret", &self.cfg.oauth_client_secret),
            ])
            .await
        {
            Ok(tokens) => {
                let access = tokens.access_token.clone();
                self.sessions.lock().unwrap().insert(
                    session_id.to_string(),
                    Session {
                        expires_at: expiry(&tokens),
                        access_token: tokens.access_token,
                        refresh_token: tokens.refresh_token.or(Some(refresh_token)),
                    },
                );
                Some(access)
            }
            Err(e) => {
                warn!(error = %e, "token refresh failed; dropping session");
                self.sessions.lock().unwrap().remove(session_id);
                None
            }
        }
    }

    pub fn logout(&self, session_id: &str) {
        self.sessions.lock().unwrap().remove(session_id);
    }

    async fn token_request(
        &self,
        ep: &Endpoints,
        form: &[(&str, &str)],
    ) -> anyhow::Result<TokenResponse> {
        let resp = self
            .http
            .post(&ep.token_endpoint)
            .form(form)
            .send()
            .await
            .with_context(|| format!("POST {}", ep.token_endpoint))?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow!("token endpoint returned {status}: {body}"));
        }
        resp.json().await.context("parsing token response")
    }
}

fn expiry(tokens: &TokenResponse) -> Instant {
    Instant::now()
        + tokens
            .expires_in
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_TOKEN_TTL)
}

/// Extract this app's session id from a request's `Cookie` header.
pub fn session_id_from_cookies(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookies = headers.get(axum::http::header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        (name == SESSION_COOKIE).then(|| value.to_string())
    })
}

/// `Set-Cookie` value opening a session. HttpOnly + SameSite=Lax: the token
/// itself stays server-side; the cookie is only an opaque handle.
pub fn session_cookie(session_id: &str) -> String {
    format!("{SESSION_COOKIE}={session_id}; HttpOnly; SameSite=Lax; Path=/")
}

/// `Set-Cookie` value clearing the session cookie.
pub fn clear_session_cookie() -> String {
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/; Max-Age=0")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkce_challenge_is_base64url_sha256() {
        // RFC 7636 appendix B test vector.
        assert_eq!(
            pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn session_cookie_roundtrip() {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::COOKIE,
            format!("other=1; {}", session_cookie("abc123"))
                .split("; HttpOnly")
                .next()
                .unwrap()
                .parse()
                .unwrap(),
        );
        assert_eq!(session_id_from_cookies(&headers).as_deref(), Some("abc123"));
    }

    #[test]
    fn missing_cookie_yields_none() {
        let headers = axum::http::HeaderMap::new();
        assert_eq!(session_id_from_cookies(&headers), None);
    }
}
