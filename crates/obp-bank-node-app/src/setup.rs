//! The `/setup` operator page: reconcile a running OBP-API instance against
//! the app config's declarative `setup` block.
//!
//! Every desired item (routing scheme, bank, account, FX rate, broker
//! registration, role grant) renders as a check — `GET`-only, no side
//! effects — plus an explicit apply action that mirrors what
//! `dev/setup_obp.sh` sends. Applies are idempotent: "already exists"
//! responses (OBP-30515 schemes, OBP-30216 entitlements, existing accounts)
//! count as success, so re-applying a converged instance is a no-op.
//!
//! The page acts as the logged-in administrator (see `oidc.rs`) with that
//! user's entitlements — there is no service credential. It never creates
//! users and never touches email validation: accounts and grants that name
//! an `owner_username`/`username` require the user to already exist, and a
//! missing user is reported as an error, not provisioned.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::warn;

use crate::oidc::{self, Oidc};
use crate::{ObpApiConfig, SetupConfig};

const SETUP_HTML: &str = include_str!("static/setup.html");
const SETUP_JS: &str = include_str!("static/setup.js");

#[derive(Clone)]
pub struct SetupState(Arc<Inner>);

struct Inner {
    cfg: ObpApiConfig,
    setup: SetupConfig,
    oidc: Oidc,
    http: reqwest::Client,
}

impl SetupState {
    pub fn new(cfg: ObpApiConfig, setup: SetupConfig) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()?;
        Ok(SetupState(Arc::new(Inner {
            oidc: Oidc::new(cfg.clone(), http.clone()),
            cfg,
            setup,
            http,
        })))
    }

    fn obp_url(&self, path: &str) -> String {
        format!("{}{}", self.0.cfg.base_url.trim_end_matches('/'), path)
    }

    /// One OBP-API call as the session's admin; returns status + JSON body
    /// (`{}` when the body is not JSON).
    async fn obp(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value), String> {
        let url = self.obp_url(path);
        let mut req = self
            .0
            .http
            .request(method.as_str().parse().unwrap(), &url);
        if let Some(token) = token {
            req = req.bearer_auth(token);
        }
        if let Some(body) = body {
            req = req.json(body);
        }
        let resp = req.send().await.map_err(|e| format!("{url}: {e}"))?;
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
        let body = resp.json::<Value>().await.unwrap_or_else(|_| json!({}));
        Ok((status, body))
    }
}

/// Routes for the setup page. With no `obp_api` config the page and its API
/// answer with an explicit "not configured" instead of vanishing, so the
/// header link never 404s mysteriously.
pub fn router(state: Option<SetupState>) -> Router {
    match state {
        Some(state) => Router::new()
            .route("/setup", get(|| async { Html(SETUP_HTML) }))
            .route(
                "/setup.js",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "application/javascript")],
                        SETUP_JS,
                    )
                }),
            )
            .route("/setup/login", get(login))
            .route("/setup/callback", get(callback))
            .route("/setup/logout", post(logout))
            .route("/api/setup/me", get(me))
            .route("/api/setup/status", get(status))
            .route("/api/setup/apply", post(apply))
            .route("/api/setup/request-entitlement", post(request_entitlement))
            .route("/api/setup/test-account", post(test_account))
            .with_state(state),
        None => {
            let not_configured = || async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({
                        "error_code": "OBP-BANK-NODE-APP-SETUP-NOT-CONFIGURED",
                        "message": "add an obp_api block to the app config to enable the setup page",
                    })),
                )
            };
            Router::new()
                .route(
                    "/setup",
                    get(|| async {
                        Html(
                            "<h1>Setup not configured</h1>\
                             <p>Add an <code>obp_api</code> block to the app config \
                             to enable this page.</p>",
                        )
                    }),
                )
                .route("/api/setup/me", get(not_configured))
                .route("/api/setup/status", get(not_configured))
                .route("/api/setup/apply", post(not_configured))
                .route("/api/setup/test-account", post(not_configured))
        }
    }
}

// ---------------------------------------------------------------------------
// Login flow

async fn login(State(state): State<SetupState>) -> Response {
    match state.0.oidc.begin_login().await {
        Ok(url) => Redirect::temporary(&url).into_response(),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-OIDC-001",
            format!("cannot start login: {e:#}"),
        ),
    }
}

#[derive(Deserialize)]
struct CallbackParams {
    #[serde(default)]
    code: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

async fn callback(
    State(state): State<SetupState>,
    Query(params): Query<CallbackParams>,
) -> Response {
    if let Some(err) = params.error {
        return error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-OIDC-002",
            format!(
                "provider refused the login: {err} {}",
                params.error_description.unwrap_or_default()
            ),
        );
    }
    let (Some(code), Some(oauth_state)) = (params.code, params.state) else {
        return error(
            StatusCode::BAD_REQUEST,
            "OBP-BANK-NODE-APP-SETUP-OIDC-003",
            "callback is missing code or state".into(),
        );
    };
    match state.0.oidc.complete_login(&oauth_state, &code).await {
        Ok(session_id) => (
            [(header::SET_COOKIE, oidc::session_cookie(&session_id))],
            Redirect::to("/setup"),
        )
            .into_response(),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-OIDC-004",
            format!("code exchange failed: {e:#}"),
        ),
    }
}

async fn logout(State(state): State<SetupState>, headers: HeaderMap) -> Response {
    if let Some(sid) = oidc::session_id_from_cookies(&headers) {
        state.0.oidc.logout(&sid);
    }
    (
        [(header::SET_COOKIE, oidc::clear_session_cookie())],
        Json(json!({ "logged_out": true })),
    )
        .into_response()
}

/// The session's access token, or the 401 the API handlers return without it.
async fn require_token(state: &SetupState, headers: &HeaderMap) -> Result<String, Response> {
    let token = match oidc::session_id_from_cookies(headers) {
        Some(sid) => state.0.oidc.access_token(&sid).await,
        None => None,
    };
    token.ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({
                "error_code": "OBP-BANK-NODE-APP-SETUP-LOGIN-REQUIRED",
                "message": "log in to OBP-API to use the setup page",
                "login_url": "/setup/login",
            })),
        )
            .into_response()
    })
}

// ---------------------------------------------------------------------------
// Current user

struct Me {
    user_id: String,
    username: String,
    entitlements: Vec<MeEntitlement>,
}

struct MeEntitlement {
    /// Empty for system roles.
    bank_id: String,
    role: String,
    /// "manual", "create_just_in_time_entitlements", or the virtual markers
    /// "super_admin_user_ids" / "oidc_operator_user_ids". Empty when the
    /// running OBP-API predates the field.
    created_by_process: String,
    /// user_id of the granter when a person made the grant; absent for
    /// system processes, virtual entitlements, and pre-field rows.
    granted_by_user_id: Option<String>,
}

async fn fetch_me(state: &SetupState, token: &str) -> Result<Me, String> {
    // v6.0.0 (not v5.1.0): its entitlements list includes the VIRTUAL
    // entitlements of `super_admin_user_ids` users (CanCreateEntitlementAt
    // One/AnyBank, CanGetAnyUser, marked created_by_process
    // "super_admin_user_ids") — without them a super admin's granting
    // rights show as missing here while grants succeed anyway.
    let (status, body) = state
        .obp(Method::GET, "/obp/v6.0.0/users/current", Some(token), None)
        .await?;
    if !status.is_success() {
        return Err(format!("GET /users/current returned {status}: {body}"));
    }
    let entitlements = body["entitlements"]["list"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|e| MeEntitlement {
                    bank_id: e["bank_id"].as_str().unwrap_or("").to_string(),
                    role: e["role_name"].as_str().unwrap_or("").to_string(),
                    created_by_process: e["created_by_process"]
                        .as_str()
                        .unwrap_or("")
                        .to_string(),
                    granted_by_user_id: e["granted_by_user_id"]
                        .as_str()
                        .map(|s| s.to_string()),
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(Me {
        user_id: body["user_id"].as_str().unwrap_or("").to_string(),
        username: body["username"].as_str().unwrap_or("").to_string(),
        entitlements,
    })
}

async fn me(State(state): State<SetupState>, headers: HeaderMap) -> Response {
    let token = match require_token(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match fetch_me(&state, &token).await {
        Ok(me) => Json(json!({
            "logged_in": true,
            "user_id": me.user_id,
            "username": me.username,
            "entitlements": me.entitlements.iter().map(|e| json!({
                "bank_id": e.bank_id,
                "role_name": e.role,
                "created_by_process": e.created_by_process,
                "granted_by_user_id": e.granted_by_user_id,
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-OBP-001",
            e,
        ),
    }
}

// ---------------------------------------------------------------------------
// Checks

/// Admin roles the configured applies will need, derived from the setup
/// block: `(bank_id, role)` with empty bank_id for system roles.
fn required_admin_roles(setup: &SetupConfig) -> Vec<(String, String)> {
    let mut roles = Vec::new();
    if !setup.routing_schemes.is_empty() {
        roles.push((String::new(), "CanCreateRoutingScheme".to_string()));
    }
    if let Some(bank) = &setup.bank {
        roles.push((String::new(), "CanCreateBank".to_string()));
        if !bank.accounts.is_empty() {
            roles.push((bank.id.clone(), "CanCreateAccount".to_string()));
        }
        if !bank.fx.is_empty() {
            roles.push((bank.id.clone(), "CanCreateFxRate".to_string()));
        }
        if bank.broker.is_some() {
            roles.push((
                bank.id.clone(),
                "CanConfigureOpenCorridorBroker".to_string(),
            ));
        }
        if !bank.role_grants.is_empty() {
            roles.push((
                bank.id.clone(),
                "CanCreateEntitlementAtOneBank".to_string(),
            ));
        }
    }
    roles
}

/// `required_role` = `(bank_id, role)` the item's Apply needs (empty bank_id
/// for system roles). The UI offers "Request role" next to Apply with it —
/// an admin without the role files an OBP entitlement request instead of
/// self-granting.
fn item(
    id: String,
    section: &str,
    title: String,
    status: &str,
    detail: String,
    required_role: (&str, &str),
) -> Value {
    json!({
        "id": id,
        "section": section,
        "title": title,
        "status": status,
        "detail": detail,
        "required_role": { "bank_id": required_role.0, "role": required_role.1 },
    })
}

/// Classify a check response: 404 (or an OBP not-found business code) means
/// "missing"; success means "ok"; anything else is surfaced verbatim.
fn classify(status: StatusCode, body: &Value, ok_detail: String) -> (&'static str, String) {
    if status.is_success() {
        return ("ok", ok_detail);
    }
    let message = body["message"].as_str().unwrap_or("").to_string();
    if status == StatusCode::NOT_FOUND || message.contains("not found") || message.contains("NotFound")
    {
        return ("missing", message);
    }
    ("error", format!("{status}: {message}"))
}

async fn status(State(state): State<SetupState>, headers: HeaderMap) -> Response {
    let token = match require_token(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let setup = &state.0.setup;
    let mut items: Vec<Value> = Vec::new();

    // OBP-API root — reachability + version, no auth involved.
    let api = match state.obp(Method::GET, "/obp/v5.1.0/root", None, None).await {
        Ok((status, body)) if status.is_success() => json!({
            "status": "ok",
            "version": body["version"],
            "git_commit": body["git_commit"],
        }),
        Ok((status, body)) => json!({ "status": "error", "detail": format!("{status}: {body}") }),
        Err(e) => json!({ "status": "error", "detail": e }),
    };

    // The admin's own entitlements vs what the applies below will need.
    let me = match fetch_me(&state, &token).await {
        Ok(me) => me,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "OBP-BANK-NODE-APP-SETUP-OBP-001",
                e,
            )
        }
    };
    for (bank_id, role) in required_admin_roles(setup) {
        // System-level CanCreateEntitlementAtAnyBank satisfies the per-bank
        // granting requirement — OBP checks the same implication.
        let satisfied_by = me.entitlements.iter().find(|e| {
            (e.bank_id == bank_id && e.role == role)
                || (role == "CanCreateEntitlementAtOneBank"
                    && e.bank_id.is_empty()
                    && e.role == "CanCreateEntitlementAtAnyBank")
        });
        items.push(item(
            format!("myrole:{bank_id}:{role}"),
            "me",
            format!(
                "{role} {}",
                if bank_id.is_empty() {
                    "(system)".to_string()
                } else {
                    format!("({bank_id})")
                }
            ),
            if satisfied_by.is_some() { "ok" } else { "missing" },
            match satisfied_by {
                // Provenance: created_by_process, plus the granter when a
                // person made the grant.
                Some(e) => match &e.granted_by_user_id {
                    Some(granter) => {
                        format!("{} · granted by {}", e.created_by_process, granter)
                    }
                    None => e.created_by_process.clone(),
                },
                None => "Apply (self-grant)".to_string(),
            },
            (&bank_id, &role),
        ));
    }

    // Routing schemes. The title carries the scheme's category — BANK
    // schemes address a bank (BIC), ACCOUNT schemes an account (IBAN, OBP);
    // both halves of a payment's routing pairs are validated against them.
    for scheme in &setup.routing_schemes {
        let name = scheme["scheme"].as_str().unwrap_or("?");
        let kind = match scheme["category"].as_str().unwrap_or("") {
            "ACCOUNT" => "Account routing scheme",
            "BANK" => "Bank routing scheme",
            _ => "Routing scheme",
        };
        let (status_str, detail) = match state
            .obp(
                Method::GET,
                &format!("/obp/v7.0.0/routing-schemes/{name}"),
                Some(&token),
                None,
            )
            .await
        {
            Ok((status, body)) => {
                let (s, d) = classify(
                    status,
                    &body,
                    format!("status {}", body["status"].as_str().unwrap_or("?")),
                );
                if s == "ok" && body["status"] != scheme["status"] {
                    (
                        "differs",
                        format!(
                            "registered but status is {} (want {})",
                            body["status"], scheme["status"]
                        ),
                    )
                } else {
                    (s, d)
                }
            }
            Err(e) => ("error", e),
        };
        items.push(item(
            format!("scheme:{name}"),
            "schemes",
            format!("{kind} {name}"),
            status_str,
            detail,
            ("", "CanCreateRoutingScheme"),
        ));
    }

    // The own bank, its accounts / FX rates / broker registration, and the
    // role grants at it.
    if let Some(bank) = &setup.bank {
        let b = &bank.id;
        let (status_str, detail) = match state
            .obp(
                Method::GET,
                &format!("/obp/v5.1.0/banks/{b}"),
                Some(&token),
                None,
            )
            .await
        {
            Ok((status, body)) => classify(
                status,
                &body,
                format!("{}", body["full_name"].as_str().unwrap_or("")),
            ),
            Err(e) => ("error", e),
        };
        items.push(item(
            format!("bank:{b}"),
            "banks",
            format!("Bank {b}"),
            status_str,
            detail,
            ("", "CanCreateBank"),
        ));

        for account in &bank.accounts {
            let a = &account.id;
            let (status_str, detail) = match state
                .obp(
                    Method::GET,
                    &format!("/obp/v5.1.0/banks/{b}/accounts/{a}/account"),
                    Some(&token),
                    None,
                )
                .await
            {
                Ok((status, body)) => {
                    let (s, d) = classify(
                        status,
                        &body,
                        format!("{}", body["balance"]["currency"].as_str().unwrap_or("")),
                    );
                    // The admin usually holds no view on accounts owned by the
                    // node service users — an authorization refusal proves
                    // nothing about existence either way.
                    if s == "error" && status.as_u16() < 500 && !d.contains("not found") {
                        (
                            "unverified",
                            format!("cannot read (no view access); Apply is idempotent — {d}"),
                        )
                    } else {
                        (s, d)
                    }
                }
                Err(e) => ("error", e),
            };
            items.push(item(
                format!("account:{b}/{a}"),
                "banks",
                format!("Account {b} / {a}"),
                status_str,
                detail,
                (b, "CanCreateAccount"),
            ));
        }

        for fx in &bank.fx {
            let (from, to) = (&fx.from_currency, &fx.to_currency);
            let (status_str, detail) = match state
                .obp(
                    Method::GET,
                    &format!("/obp/v2.2.0/banks/{b}/fx/{from}/{to}"),
                    Some(&token),
                    None,
                )
                .await
            {
                Ok((status, body)) => {
                    if status.is_success() {
                        ("ok", format!("rate {}", body["conversion_value"]))
                    } else {
                        (
                            "missing",
                            body["message"].as_str().unwrap_or("").to_string(),
                        )
                    }
                }
                Err(e) => ("error", e),
            };
            items.push(item(
                format!("fx:{b}:{from}:{to}"),
                "banks",
                format!("FX {from}→{to} ({b})"),
                status_str,
                detail,
                (b, "CanCreateFxRate"),
            ));
        }

        if let Some(broker) = &bank.broker {
            let (status_str, detail) = match state
                .obp(
                    Method::GET,
                    &format!("/obp/v7.0.0/banks/{b}/open-corridor/broker"),
                    Some(&token),
                    None,
                )
                .await
            {
                Ok((status, body)) if status.is_success() => {
                    // Compare the desired fields OBP-API reads back (it never
                    // returns the password, so that stays unchecked).
                    let mismatched: Vec<String> = broker
                        .as_object()
                        .map(|o| {
                            o.iter()
                                .filter(|(k, v)| *k != "password" && body[*k] != **v)
                                .map(|(k, _)| k.clone())
                                .collect()
                        })
                        .unwrap_or_default();
                    if mismatched.is_empty() {
                        ("ok", format!("vhost {}", body["virtual_host"]))
                    } else {
                        ("differs", format!("differs on: {}", mismatched.join(", ")))
                    }
                }
                Ok((status, body)) => classify(status, &body, String::new()),
                Err(e) => ("error", e),
            };
            items.push(item(
                format!("broker:{b}"),
                "banks",
                format!("Open Corridor broker ({b})"),
                status_str,
                detail,
                (b, "CanConfigureOpenCorridorBroker"),
            ));
        }

        // Role grants at the own bank for pre-existing users (never
        // created here).
        for grant in &bank.role_grants {
            let (status_str, detail) = match state
                .obp(
                    Method::GET,
                    &format!("/obp/v5.1.0/users/username/{}", grant.username),
                    Some(&token),
                    None,
                )
                .await
            {
                Ok((status, body)) if status.is_success() => {
                    let held = body["entitlements"]["list"]
                        .as_array()
                        .map(|list| {
                            list.iter().any(|e| {
                                e["bank_id"].as_str().unwrap_or("") == *b
                                    && e["role_name"].as_str().unwrap_or("") == grant.role
                            })
                        })
                        .unwrap_or(false);
                    if held {
                        ("ok", String::new())
                    } else {
                        ("missing", String::new())
                    }
                }
                Ok((status, body)) => {
                    let (s, d) = classify(status, &body, String::new());
                    if s == "missing" {
                        (
                            "error",
                            format!(
                                "user {} does not exist — service users are provisioned outside this app",
                                grant.username
                            ),
                        )
                    } else {
                        (s, d)
                    }
                }
                Err(e) => ("error", e),
            };
            items.push(item(
                format!("grant:{}@{}:{}", grant.username, b, grant.role),
                "grants",
                format!("{} for {} ({})", grant.role, grant.username, b),
                status_str,
                detail,
                // Granting an entitlement to another user needs granting
                // rights at the bank (or superadmin).
                (b, "CanCreateEntitlementAtOneBank"),
            ));
        }
    }

    Json(json!({
        "bank": setup.bank.as_ref().map(|b| b.id.clone()),
        "generated_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        "obp_api": {
            "base_url": state.0.cfg.base_url,
            "oauth_provider": state.0.cfg.oauth_provider,
        },
        "api": api,
        "me": { "user_id": me.user_id, "username": me.username },
        "items": items,
    }))
    .into_response()
}

// ---------------------------------------------------------------------------
// Applies

#[derive(Deserialize)]
struct ApplyRequest {
    id: String,
}

/// Resolve an existing username to a user_id — a missing user is an error by
/// design (this page never registers users).
async fn resolve_user_id(state: &SetupState, token: &str, username: &str) -> Result<String, String> {
    let (status, body) = state
        .obp(
            Method::GET,
            &format!("/obp/v5.1.0/users/username/{username}"),
            Some(token),
            None,
        )
        .await?;
    if !status.is_success() {
        return Err(format!(
            "user {username} does not exist — service users are provisioned outside this app ({status})"
        ));
    }
    body["user_id"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| format!("no user_id in the response for {username}"))
}

async fn create_account(
    state: &SetupState,
    token: &str,
    me: &Me,
    bank_id: &str,
    account_id: &str,
    label: &str,
    currency: &str,
    owner_username: Option<&str>,
) -> Result<(StatusCode, Value), String> {
    let owner = match owner_username {
        Some(username) => resolve_user_id(state, token, username).await?,
        None => me.user_id.clone(),
    };
    state
        .obp(
            Method::PUT,
            &format!("/obp/v5.1.0/banks/{bank_id}/accounts/{account_id}"),
            Some(token),
            Some(&json!({
                "user_id": owner,
                "label": label,
                "product_code": "OPEN_CORRIDOR",
                "balance": { "currency": currency, "amount": "0" },
                "branch_id": "",
                "account_routings": [],
            })),
        )
        .await
}

async fn grant_role(
    state: &SetupState,
    token: &str,
    user_id: &str,
    bank_id: &str,
    role: &str,
) -> Result<(StatusCode, Value), String> {
    state
        .obp(
            Method::POST,
            &format!("/obp/v5.1.0/users/{user_id}/entitlements"),
            Some(token),
            Some(&json!({ "bank_id": bank_id, "role_name": role })),
        )
        .await
}

/// "Already exists" style refusals that make an apply a successful no-op.
fn already_ok(body: &Value) -> bool {
    let message = body["message"].as_str().unwrap_or("");
    [
        "OBP-30515",
        "OBP-30216",
        "OBP-30208",
        "already exists",
        "EntitlementRequestAlreadyExists",
        "Entitlement Request already exists",
    ]
    .iter()
    .any(|code| message.contains(code))
}

#[derive(Deserialize)]
struct RequestEntitlementRequest {
    #[serde(default)]
    bank_id: String,
    role: String,
}

/// File an OBP entitlement request for the logged-in admin — the path for an
/// admin who lacks a role and cannot self-grant. Someone with granting
/// rights approves it in OBP (e.g. via the API Manager). A duplicate pending
/// request counts as success.
async fn request_entitlement(
    State(state): State<SetupState>,
    headers: HeaderMap,
    Json(req): Json<RequestEntitlementRequest>,
) -> Response {
    let token = match require_token(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    match state
        .obp(
            Method::POST,
            "/obp/v3.0.0/entitlement-requests",
            Some(&token),
            Some(&json!({ "bank_id": req.bank_id, "role_name": req.role })),
        )
        .await
    {
        Ok((status, body)) => Json(json!({
            "ok": status.is_success() || already_ok(&body),
            "status_code": status.as_u16(),
            "obp": body,
        }))
        .into_response(),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-APPLY-001",
            e,
        ),
    }
}

async fn apply(
    State(state): State<SetupState>,
    headers: HeaderMap,
    Json(req): Json<ApplyRequest>,
) -> Response {
    let token = match require_token(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let me = match fetch_me(&state, &token).await {
        Ok(me) => me,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "OBP-BANK-NODE-APP-SETUP-OBP-001",
                e,
            )
        }
    };
    let setup = &state.0.setup;

    // The action is re-derived from config by item id — the browser only ever
    // names an item, it never supplies a body.
    let result: Result<(StatusCode, Value), String> = match req.id.split_once(':') {
        Some(("scheme", name)) => match setup
            .routing_schemes
            .iter()
            .find(|s| s["scheme"].as_str() == Some(name))
        {
            Some(scheme) => {
                state
                    .obp(
                        Method::POST,
                        "/obp/v7.0.0/routing-schemes",
                        Some(&token),
                        Some(scheme),
                    )
                    .await
            }
            None => Err(format!("no routing scheme {name} in the setup config")),
        },
        Some(("bank", bank_id)) => match setup.bank.as_ref().filter(|b| b.id == bank_id) {
            Some(bank) => {
                state
                    .obp(
                        Method::POST,
                        "/obp/v5.0.0/banks",
                        Some(&token),
                        Some(&json!({
                            "id": bank.id,
                            "bank_code": bank.id,
                            "full_name": bank.full_name.clone().unwrap_or_else(|| bank.id.clone()),
                            "logo": "",
                            "website": "",
                            "bank_routings": [],
                        })),
                    )
                    .await
            }
            None => Err(format!("{bank_id} is not this instance's bank")),
        },
        Some(("account", rest)) => match rest.split_once('/') {
            Some((bank_id, account_id)) => {
                match setup
                    .bank
                    .as_ref()
                    .filter(|b| b.id == bank_id)
                    .and_then(|b| b.accounts.iter().find(|a| a.id == account_id))
                {
                    Some(account) => {
                        create_account(
                            &state,
                            &token,
                            &me,
                            bank_id,
                            account_id,
                            &account.label,
                            &account.currency,
                            account.owner_username.as_deref(),
                        )
                        .await
                    }
                    None => Err(format!("no account {rest} in the setup config")),
                }
            }
            None => Err(format!("malformed account id {rest}")),
        },
        Some(("fx", rest)) => {
            let parts: Vec<&str> = rest.split(':').collect();
            match parts.as_slice() {
                [bank_id, from, to] => {
                    match setup
                        .bank
                        .as_ref()
                        .filter(|b| b.id == *bank_id)
                        .and_then(|b| {
                            b.fx.iter()
                                .find(|f| f.from_currency == *from && f.to_currency == *to)
                        }) {
                        Some(fx) => {
                            state
                                .obp(
                                    Method::PUT,
                                    &format!("/obp/v2.2.0/banks/{bank_id}/fx"),
                                    Some(&token),
                                    Some(&json!({
                                        "bank_id": bank_id,
                                        "from_currency_code": fx.from_currency,
                                        "to_currency_code": fx.to_currency,
                                        "conversion_value": fx.rate,
                                        "inverse_conversion_value": fx.inverse_rate,
                                        "effective_date": format!(
                                            "{}",
                                            chrono::Utc::now().format("%Y-%m-%dT00:00:00Z")
                                        ),
                                    })),
                                )
                                .await
                        }
                        None => Err(format!("no FX rate {rest} in the setup config")),
                    }
                }
                _ => Err(format!("malformed fx id {rest}")),
            }
        }
        Some(("broker", bank_id)) => match setup
            .bank
            .as_ref()
            .filter(|b| b.id == bank_id)
            .and_then(|b| b.broker.as_ref())
        {
            Some(broker) => {
                state
                    .obp(
                        Method::PUT,
                        &format!("/obp/v7.0.0/banks/{bank_id}/open-corridor/broker"),
                        Some(&token),
                        Some(broker),
                    )
                    .await
            }
            None => Err(format!("no broker for bank {bank_id} in the setup config")),
        },
        Some(("grant", rest)) => {
            match rest
                .split_once('@')
                .and_then(|(user, rest)| rest.split_once(':').map(|(b, r)| (user, b, r)))
            {
                Some((username, bank_id, role)) => {
                    match setup.bank.as_ref().filter(|b| b.id == bank_id).and_then(|b| {
                        b.role_grants
                            .iter()
                            .find(|g| g.username == username && g.role == role)
                    }) {
                        Some(grant) => match resolve_user_id(&state, &token, &grant.username).await
                        {
                            Ok(user_id) => {
                                grant_role(&state, &token, &user_id, bank_id, &grant.role).await
                            }
                            Err(e) => Err(e),
                        },
                        None => Err(format!("no role grant {rest} in the setup config")),
                    }
                }
                None => Err(format!("malformed grant id {rest}")),
            }
        }
        Some(("myrole", rest)) => match rest.split_once(':') {
            Some((bank_id, role))
                if required_admin_roles(setup)
                    .iter()
                    .any(|(b, r)| b == bank_id && r == role) =>
            {
                grant_role(&state, &token, &me.user_id, bank_id, role).await
            }
            _ => Err(format!("{rest} is not a role this setup config needs")),
        },
        _ => Err(format!("unknown item id {}", req.id)),
    };

    match result {
        Ok((status, body)) => {
            let ok = status.is_success() || already_ok(&body);
            if !ok {
                warn!(id = %req.id, %status, "setup apply refused by OBP-API");
            }
            Json(json!({
                "id": req.id,
                "ok": ok,
                "status_code": status.as_u16(),
                "obp": body,
            }))
            .into_response()
        }
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-APPLY-001",
            e,
        ),
    }
}

// ---------------------------------------------------------------------------
// Test accounts

#[derive(Deserialize)]
struct TestAccountRequest {
    bank_id: String,
    account_id: String,
    #[serde(default)]
    label: String,
    currency: String,
    #[serde(default)]
    owner_username: Option<String>,
}

/// Free-form test-account creation (same call as the account apply) — for
/// seeding demo customer accounts beyond the declared corridor accounts.
async fn test_account(
    State(state): State<SetupState>,
    headers: HeaderMap,
    Json(req): Json<TestAccountRequest>,
) -> Response {
    let token = match require_token(&state, &headers).await {
        Ok(t) => t,
        Err(resp) => return resp,
    };
    let me = match fetch_me(&state, &token).await {
        Ok(me) => me,
        Err(e) => {
            return error(
                StatusCode::BAD_GATEWAY,
                "OBP-BANK-NODE-APP-SETUP-OBP-001",
                e,
            )
        }
    };
    match create_account(
        &state,
        &token,
        &me,
        &req.bank_id,
        &req.account_id,
        &req.label,
        &req.currency,
        req.owner_username.as_deref(),
    )
    .await
    {
        Ok((status, body)) => Json(json!({
            "ok": status.is_success() || already_ok(&body),
            "status_code": status.as_u16(),
            "obp": body,
        }))
        .into_response(),
        Err(e) => error(
            StatusCode::BAD_GATEWAY,
            "OBP-BANK-NODE-APP-SETUP-APPLY-001",
            e,
        ),
    }
}

fn error(status: StatusCode, code: &str, message: String) -> Response {
    (
        status,
        Json(json!({ "error_code": code, "message": message })),
    )
        .into_response()
}
