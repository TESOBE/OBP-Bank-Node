//! Setup-page tests: the OIDC login flow (well-known → discovery →
//! authorize redirect → code exchange, PKCE verified end-to-end) and the
//! check/apply API — against a stub OBP-API + OIDC provider on an ephemeral
//! port, since the app reaches both over real HTTP.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use axum::body::{to_bytes, Body};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, Method, Request, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post, put};
use axum::{Form, Json, Router};
use base64::Engine;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use crate::proxy::{build_router, AppState};
use crate::setup::SetupState;
use crate::{NodeConfig, ObpApiConfig, SetupAccount, SetupBank, SetupConfig, SetupNode, SetupRoleGrant};

const ACCESS_TOKEN: &str = "test-access-token";

/// Everything the stub records for assertions.
#[derive(Default)]
struct Captured {
    token_forms: Vec<HashMap<String, String>>,
    scheme_posts: Vec<Value>,
    entitlement_posts: Vec<(String, Value)>,
    account_puts: Vec<(String, Value)>,
    account_posts: Vec<(String, Value)>,
    entitlement_requests: Vec<Value>,
    /// POSTs made for the node's Consumer: (path, body).
    node_posts: Vec<(String, Value)>,
}

fn record_node_post(s: &Stub, path: String, body: Value) {
    s.captured.lock().unwrap().node_posts.push((path, body));
}

#[derive(Clone)]
struct Stub {
    base: String,
    captured: Arc<Mutex<Captured>>,
}

fn bearer_ok(headers: &HeaderMap) -> bool {
    headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {ACCESS_TOKEN}"))
}

/// Stub OBP-API + OIDC provider. Known world: bank `rt.bank.a` (with account
/// `settlement-a` and a registered broker), scheme `OBP`; bank `rt.bank.b`,
/// scheme `IBAN`, all FX rates, and every entitlement are absent. User
/// `rt.node.a` exists with no entitlements; `ghost` does not exist.
fn obp_stub(base: String, captured: Arc<Mutex<Captured>>) -> Router {
    let state = Stub { base, captured };
    Router::new()
        .route(
            "/obp/v5.1.0/well-known",
            get(|State(s): State<Stub>| async move {
                Json(json!({ "well_known_uris": [
                    { "provider": "google", "url": format!("{}/nope", s.base) },
                    { "provider": "obp-oidc", "url": format!("{}/discovery", s.base) },
                ]}))
            }),
        )
        .route(
            "/discovery",
            get(|State(s): State<Stub>| async move {
                Json(json!({
                    "authorization_endpoint": format!("{}/authorize", s.base),
                    "token_endpoint": format!("{}/token", s.base),
                }))
            }),
        )
        .route(
            "/token",
            post(
                |State(s): State<Stub>, Form(form): Form<HashMap<String, String>>| async move {
                    s.captured.lock().unwrap().token_forms.push(form);
                    Json(json!({
                        "access_token": ACCESS_TOKEN,
                        "refresh_token": "test-refresh-token",
                        "token_type": "Bearer",
                        "expires_in": 3600,
                        "id_token": "x.y.z",
                    }))
                },
            ),
        )
        .route(
            "/obp/v5.1.0/root",
            get(|| async { Json(json!({ "version": "v7.0.0", "git_commit": "deadbeef" })) }),
        )
        .route(
            "/obp/v6.0.0/users/current",
            get(|headers: HeaderMap| async move {
                if !bearer_ok(&headers) {
                    return (StatusCode::UNAUTHORIZED, Json(json!({ "message": "OBP-20001" })))
                        .into_response();
                }
                // v6.0.0 includes super-admin VIRTUAL entitlements in the
                // list, marked by created_by_process.
                Json(json!({
                    "user_id": "admin-user-id",
                    "username": "admin",
                    "entitlements": { "list": [
                        { "bank_id": "", "role_name": "CanCreateBank",
                          "created_by_process": "manual",
                          "granted_by_user_id": "granter-user-id" },
                        { "bank_id": "", "role_name": "CanCreateEntitlementAtAnyBank",
                          "created_by_process": "super_admin_user_ids" },
                    ]},
                }))
                .into_response()
            }),
        )
        .route(
            "/obp/v5.1.0/users/username/:username",
            get(|Path(username): Path<String>| async move {
                match username.as_str() {
                    "rt.node.a" => Json(json!({
                        "user_id": "node-a-user-id",
                        "username": "rt.node.a",
                        "entitlements": { "list": [] },
                    }))
                    .into_response(),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "OBP-20005: User not found" })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/obp/v5.1.0/users/:user_id/entitlements",
            post(
                |State(s): State<Stub>, Path(user_id): Path<String>, Json(body): Json<Value>| async move {
                    s.captured
                        .lock()
                        .unwrap()
                        .entitlement_posts
                        .push((user_id, body));
                    (StatusCode::CREATED, Json(json!({ "entitlement_id": "e1" })))
                },
            ),
        )
        .route(
            "/obp/v3.0.0/entitlement-requests",
            post(|State(s): State<Stub>, Json(body): Json<Value>| async move {
                s.captured
                    .lock()
                    .unwrap()
                    .entitlement_requests
                    .push(body.clone());
                // A pending duplicate exists for this one role.
                if body["role_name"] == "CanCreateBank" {
                    return (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "message": "OBP-30217: EntitlementRequestAlreadyExists" })),
                    )
                        .into_response();
                }
                (
                    StatusCode::CREATED,
                    Json(json!({ "entitlement_request_id": "er1" })),
                )
                    .into_response()
            }),
        )
        .route(
            "/obp/v7.0.0/routing-schemes",
            post(|State(s): State<Stub>, Json(body): Json<Value>| async move {
                s.captured.lock().unwrap().scheme_posts.push(body);
                (StatusCode::CREATED, Json(json!({ "scheme": "IBAN" })))
            }),
        )
        .route(
            "/obp/v7.0.0/routing-schemes/:scheme",
            get(|Path(scheme): Path<String>| async move {
                match scheme.as_str() {
                    "OBP" => Json(json!({ "scheme": "OBP", "status": "ACTIVE" })).into_response(),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "OBP-30514: Routing scheme not found" })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/obp/v5.1.0/banks/:bank_id",
            get(|Path(bank_id): Path<String>| async move {
                match bank_id.as_str() {
                    "rt.bank.a" => {
                        Json(json!({ "id": "rt.bank.a", "full_name": "Bank A" })).into_response()
                    }
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "OBP-30001: Bank not found" })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/obp/v6.0.0/banks/:bank_id/account-directory",
            get(|Path(bank_id): Path<String>| async move {
                match bank_id.as_str() {
                    "rt.bank.a" => Json(json!({
                        "accounts": [
                            {
                                "account_id": "settlement-a",
                                "label": "Node A corridor account",
                                "account_routings": [
                                    { "scheme": "OBP", "address": "settlement-a" },
                                    { "scheme": "CARDANO", "address": "addr_test1aaa" },
                                ],
                            },
                        ],
                    }))
                    .into_response(),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "OBP-30001: Bank not found" })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/obp/v7.0.0/banks/:bank_id/accounts/:account_id",
            put(
                |State(s): State<Stub>,
                 Path((bank_id, account_id)): Path<(String, String)>,
                 Json(body): Json<Value>| async move {
                    s.captured
                        .lock()
                        .unwrap()
                        .account_puts
                        .push((format!("{bank_id}/{account_id}"), body));
                    Json(json!({ "account_id": account_id }))
                },
            ),
        )
        .route(
            "/obp/v7.0.0/banks/:bank_id/accounts",
            post(
                |State(s): State<Stub>,
                 Path(bank_id): Path<String>,
                 Json(body): Json<Value>| async move {
                    s.captured
                        .lock()
                        .unwrap()
                        .account_posts
                        .push((bank_id, body));
                    Json(json!({ "account_id": "generated-account-id" }))
                },
            ),
        )
        .route(
            "/obp/v2.2.0/banks/:bank_id/fx/:from/:to",
            get(|| async {
                (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "message": "OBP-10004: ISO Currency code combination not supported for FX." })),
                )
            }),
        )
        // The node's Consumer: obp-bank-node-a exists, is not yet a Platform
        // App, holds CanAttachOpenCorridorPromise at rt.bank.a only, and has
        // access to its view on settlement-a only; settlement-a-usd does not
        // exist.
        .route(
            "/obp/v6.0.0/management/consumers",
            get(|| async {
                Json(json!({ "consumers": [
                    { "app_name": "obp-portal-client", "consumer_id": "portal-consumer-id" },
                    { "app_name": "obp-bank-node-a", "consumer_id": "node-a-consumer-id" },
                ]}))
            }),
        )
        .route(
            "/obp/v7.0.0/management/platform-apps",
            get(|| async { Json(json!({ "platform_apps": [] })) }).post(
                |State(s): State<Stub>, Json(body): Json<Value>| async move {
                    record_node_post(&s, "platform-apps".into(), body.clone());
                    (StatusCode::CREATED, Json(json!({ "consumer_id": body["consumer_id"], "state": "not_declared" })))
                },
            ),
        )
        .route(
            "/obp/v4.0.0/consumers/:consumer_id/scopes",
            get(|Path(consumer_id): Path<String>| async move {
                assert_eq!(consumer_id, "node-a-consumer-id");
                Json(json!({ "list": [
                    { "scope_id": "s1", "role_name": "CanAttachOpenCorridorPromise", "bank_id": "rt.bank.a" },
                    { "scope_id": "s2", "role_name": "CanSettleOpenCorridor", "bank_id": "other.bank" },
                ]}))
            }),
        )
        .route(
            "/obp/v7.0.0/consumers/:consumer_id/scopes",
            post(
                |State(s): State<Stub>, Path(consumer_id): Path<String>, Json(body): Json<Value>| async move {
                    record_node_post(&s, format!("scopes:{consumer_id}"), body);
                    (StatusCode::CREATED, Json(json!({ "scope_id": "s3" })))
                },
            ),
        )
        .route(
            "/obp/v7.0.0/banks/:bank_id/accounts/:account_id/views/owner/consumer-account-access",
            get(|Path((_, account_id)): Path<(String, String)>| async move {
                match account_id.as_str() {
                    "settlement-a" => Json(json!({ "consumer_account_access": [
                        { "consumer_id": "node-a-consumer-id", "view_id": "_open-corridor-node" },
                    ]}))
                    .into_response(),
                    "settlement-a-ngn" => Json(json!({ "consumer_account_access": [
                        { "consumer_id": "someone-else", "view_id": "_open-corridor-node" },
                    ]}))
                    .into_response(),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "message": "OBP-30018: Bank Account not found." })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/obp/v5.1.0/banks/:bank_id/accounts/:account_id/views/owner/account-access/grant",
            post(
                |State(s): State<Stub>, Path((_, a)): Path<(String, String)>, Json(body): Json<Value>| async move {
                    record_node_post(&s, format!("grant-view:{a}"), body);
                    (StatusCode::CREATED, Json(json!({ "id": "ManageCustomViews" })))
                },
            ),
        )
        .route(
            "/obp/v6.0.0/management/banks/:bank_id/accounts/:account_id/views",
            post(
                |State(s): State<Stub>, Path((_, a)): Path<(String, String)>, Json(body): Json<Value>| async move {
                    record_node_post(&s, format!("create-view:{a}"), body);
                    // The view is already there: an apply must go on to the grant.
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "message": "OBP-30266: The custom view is already exists." })),
                    )
                },
            ),
        )
        .route(
            "/obp/v7.0.0/banks/:bank_id/accounts/:account_id/views/ManageCustomViews/consumer-account-access",
            post(
                |State(s): State<Stub>, Path((_, a)): Path<(String, String)>, Json(body): Json<Value>| async move {
                    record_node_post(&s, format!("consumer-access:{a}"), body);
                    (StatusCode::CREATED, Json(json!({ "consumer_account_access_id": "caa1" })))
                },
            ),
        )
        .route(
            "/obp/v7.0.0/banks/:bank_id/amqp-broker",
            get(|| async {
                Json(json!({
                    "bank_id": "rt.bank.a",
                    "host": "localhost",
                    "port": 5672,
                    "virtual_host": "/bank.rt.bank.a",
                    "username": "bank_a_node",
                    "use_ssl": false,
                }))
            }),
        )
        .with_state(state)
}

async fn spawn_obp_stub(captured: Arc<Mutex<Captured>>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = obp_stub(base.clone(), captured);
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    base
}

fn setup_config() -> SetupConfig {
    SetupConfig {
        routing_schemes: vec![
            json!({ "scheme": "OBP", "country": "INT", "category": "ACCOUNT", "status": "ACTIVE" }),
            json!({ "scheme": "IBAN", "country": "INT", "category": "ACCOUNT", "status": "ACTIVE" }),
        ],
        bank: Some(SetupBank {
            id: "rt.bank.a".into(),
            full_name: Some("Round-trip rt.bank.a".into()),
            accounts: vec![SetupAccount {
                id: "settlement-a".into(),
                label: "Node A corridor account".into(),
                currency: "KES".into(),
                owner_username: Some("rt.node.a".into()),
                routings: vec![crate::SetupRouting {
                    scheme: "CARDANO".into(),
                    address: "addr_test1aaa".into(),
                }],
            }],
            fx: vec![crate::SetupFxRate {
                from_currency: "EUR".into(),
                to_currency: "KES".into(),
                rate: 150.0,
                inverse_rate: 0.006667,
            }],
            broker: Some(json!({
                "host": "localhost",
                "port": 5672,
                "virtual_host": "/bank.rt.bank.a",
                "username": "bank_a_node",
                "password": "secret",
                "use_ssl": false,
            })),
            role_grants: vec![
                SetupRoleGrant {
                    username: "rt.node.a".into(),
                    role: "CanSettleOpenCorridor".into(),
                },
                SetupRoleGrant {
                    username: "ghost".into(),
                    role: "CanSettleOpenCorridor".into(),
                },
            ],
            node: None,
        }),
    }
}

fn node_setup_config(consumer_name: &str) -> SetupConfig {
    let mut setup = setup_config();
    setup.bank.as_mut().unwrap().node = Some(SetupNode {
        consumer_name: consumer_name.into(),
        label: "OBP Bank Node A (rt.bank.a)".into(),
        scopes: vec!["CanAttachOpenCorridorPromise".into(), "CanSettleOpenCorridor".into()],
        view_id: "_open-corridor-node".into(),
        corridor_accounts: vec![
            "settlement-a".into(),
            "settlement-a-ngn".into(),
            "settlement-a-usd".into(),
        ],
    });
    setup
}

fn app_with_setup(obp_base: &str) -> Router {
    app_with_config(obp_base, setup_config())
}

fn app_with_config(obp_base: &str, setup: SetupConfig) -> Router {
    let cfg = ObpApiConfig {
        base_url: obp_base.to_string(),
        oauth_provider: "obp-oidc".into(),
        oauth_client_id: "app-client".into(),
        oauth_client_secret: "app-secret".into(),
        callback_url: "http://localhost:8091/setup/callback".into(),
        discovery_url: None,
    };
    let nodes = vec![NodeConfig {
        name: "node-a".into(),
        base_url: "http://127.0.0.1:1".into(),
        bearer_token: None,
    }];
    build_router(
        AppState::new(nodes, Default::default()).unwrap(),
        Some(SetupState::new(cfg, setup).unwrap()),
    )
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn get_req(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn get_with_cookie(uri: &str, cookie: &str) -> Request<Body> {
    Request::builder()
        .uri(uri)
        .header(header::COOKIE, cookie)
        .body(Body::empty())
        .unwrap()
}

fn post_json_with_cookie(uri: &str, cookie: &str, body: Value) -> Request<Body> {
    Request::builder()
        .method(Method::POST)
        .uri(uri)
        .header(header::COOKIE, cookie)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .unwrap()
}

/// Drive the full login flow against `app`, returning the session cookie.
/// Asserts the PKCE contract on the way: the verifier sent to the token
/// endpoint must hash (S256) to the challenge sent to the authorize endpoint.
async fn log_in(app: &Router, captured: &Arc<Mutex<Captured>>) -> String {
    let resp = app.clone().oneshot(get_req("/setup/login")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = resp.headers()[header::LOCATION].to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    assert_eq!(params["response_type"], "code");
    assert_eq!(params["client_id"], "app-client");
    assert_eq!(params["redirect_uri"], "http://localhost:8091/setup/callback");
    assert_eq!(params["code_challenge_method"], "S256");

    let resp = app
        .clone()
        .oneshot(get_req(&format!(
            "/setup/callback?code=test-code&state={}",
            params["state"]
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    // Without a `next` on the login the callback returns to /setup.
    assert_eq!(resp.headers()[header::LOCATION], "/setup");
    let cookie = resp.headers()[header::SET_COOKIE]
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string();

    let form = captured.lock().unwrap().token_forms.last().unwrap().clone();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["code"], "test-code");
    assert_eq!(form["client_id"], "app-client");
    assert_eq!(form["client_secret"], "app-secret");
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(Sha256::digest(form["code_verifier"].as_bytes()));
    assert_eq!(challenge, params["code_challenge"], "PKCE challenge mismatch");
    cookie
}

#[tokio::test]
async fn setup_reports_not_configured_without_an_obp_api_block() {
    let app = build_router(
        AppState::new(
            vec![NodeConfig {
                name: "node-a".into(),
                base_url: "http://127.0.0.1:1".into(),
                bearer_token: None,
            }],
            Default::default(),
        )
        .unwrap(),
        None,
    );
    let resp = app.clone().oneshot(get_req("/setup")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let resp = app.clone().oneshot(get_req("/accounts")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let resp = app.clone().oneshot(get_req("/api/setup/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-UI-SETUP-NOT-CONFIGURED");

    let resp = app
        .oneshot(get_req("/api/setup/account-directory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn setup_api_requires_login() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured).await;
    let app = app_with_setup(&base);
    let resp = app.oneshot(get_req("/api/setup/status")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-UI-SETUP-LOGIN-REQUIRED");
    assert_eq!(v["login_url"], "/setup/login");
}

#[tokio::test]
async fn login_flow_opens_a_session_and_me_reports_the_admin() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .oneshot(get_with_cookie("/api/setup/me", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["logged_in"], true);
    assert_eq!(v["username"], "admin");
    assert_eq!(v["user_id"], "admin-user-id");
}

#[tokio::test]
async fn callback_with_unknown_state_is_refused() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured).await;
    let app = app_with_setup(&base);
    let resp = app
        .oneshot(get_req("/setup/callback?code=x&state=forged"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_json(resp).await;
    assert_eq!(v["error_code"], "OBP-BANK-NODE-UI-SETUP-OIDC-004");
}

#[tokio::test]
async fn status_classifies_present_missing_and_broken_items() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .oneshot(get_with_cookie("/api/setup/status", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["api"]["status"], "ok");
    assert_eq!(v["api"]["version"], "v7.0.0");
    assert_eq!(v["me"]["username"], "admin");

    let by_id: HashMap<String, Value> = v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["id"].as_str().unwrap().to_string(), i.clone()))
        .collect();

    // One instance = one bank: only the own bank's items exist.
    assert_eq!(v["bank"], "rt.bank.a");
    assert!(!by_id.keys().any(|k| k.contains("rt.bank.b")));

    assert_eq!(by_id["scheme:OBP"]["status"], "ok");
    assert_eq!(by_id["scheme:IBAN"]["status"], "missing");
    assert_eq!(by_id["bank:rt.bank.a"]["status"], "ok");
    assert_eq!(by_id["account:rt.bank.a/settlement-a"]["status"], "ok");
    assert_eq!(by_id["fx:rt.bank.a:EUR:KES"]["status"], "missing");
    // Broker registered and matching on every non-password field.
    assert_eq!(by_id["broker:rt.bank.a"]["status"], "ok");
    // The admin holds CanCreateBank but not CanCreateRoutingScheme; held
    // items show how the entitlement came to be.
    assert_eq!(by_id["myrole::CanCreateBank"]["status"], "ok");
    assert_eq!(
        by_id["myrole::CanCreateBank"]["detail"],
        "manual · granted by granter-user-id"
    );
    assert_eq!(by_id["myrole::CanCreateRoutingScheme"]["status"], "missing");
    // The virtual CanCreateEntitlementAtAnyBank (super admin) satisfies the
    // per-bank granting requirement, and the detail says so.
    assert_eq!(
        by_id["myrole:rt.bank.a:CanCreateEntitlementAtOneBank"]["status"],
        "ok"
    );
    assert_eq!(
        by_id["myrole:rt.bank.a:CanCreateEntitlementAtOneBank"]["detail"],
        "super_admin_user_ids"
    );
    // rt.node.a exists without the role; ghost does not exist — and the page
    // must say so rather than offer to create it.
    assert_eq!(
        by_id["grant:rt.node.a@rt.bank.a:CanSettleOpenCorridor"]["status"],
        "missing"
    );
    let ghost = &by_id["grant:ghost@rt.bank.a:CanSettleOpenCorridor"];
    assert_eq!(ghost["status"], "error");
    assert!(ghost["detail"]
        .as_str()
        .unwrap()
        .contains("provisioned outside this app"));

    // Every item names the role its Apply needs, so the UI can offer
    // "Request role" next to Apply.
    assert_eq!(
        by_id["scheme:IBAN"]["required_role"],
        json!({ "bank_id": "", "role": "CanCreateRoutingScheme" })
    );
    assert_eq!(
        by_id["account:rt.bank.a/settlement-a"]["required_role"],
        json!({ "bank_id": "rt.bank.a", "role": "CanCreateAccount" })
    );
    assert_eq!(
        by_id["grant:rt.node.a@rt.bank.a:CanSettleOpenCorridor"]["required_role"],
        json!({ "bank_id": "rt.bank.a", "role": "CanCreateEntitlementAtOneBank" })
    );

    // The snapshot block the UI copies for other agents.
    assert!(v["generated_at"].as_str().is_some());
    assert_eq!(v["obp_api"]["oauth_provider"], "obp-oidc");
}

/// Start a login with the given query string and drive the callback,
/// returning the callback's redirect target.
async fn login_redirect_target(app: &Router, login_uri: &str) -> String {
    let resp = app.clone().oneshot(get_req(login_uri)).await.unwrap();
    assert_eq!(resp.status(), StatusCode::TEMPORARY_REDIRECT);
    let location = resp.headers()[header::LOCATION].to_str().unwrap();
    let url = reqwest::Url::parse(location).unwrap();
    let params: HashMap<String, String> = url.query_pairs().into_owned().collect();
    let resp = app
        .clone()
        .oneshot(get_req(&format!(
            "/setup/callback?code=test-code&state={}",
            params["state"]
        )))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    resp.headers()[header::LOCATION].to_str().unwrap().to_string()
}

#[tokio::test]
async fn login_next_returns_to_the_named_page_and_refuses_external_urls() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured).await;
    let app = app_with_setup(&base);

    // A same-app path comes back after the callback.
    assert_eq!(
        login_redirect_target(&app, "/setup/login?next=/accounts").await,
        "/accounts"
    );
    // Anything that could leave the app falls back to /setup.
    assert_eq!(
        login_redirect_target(&app, "/setup/login?next=//evil.example/x").await,
        "/setup"
    );
    assert_eq!(
        login_redirect_target(&app, "/setup/login?next=https%3A%2F%2Fevil.example").await,
        "/setup"
    );
}

#[tokio::test]
async fn account_directory_requires_login_and_relays_the_directory() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);

    let resp = app
        .clone()
        .oneshot(get_req("/api/setup/account-directory"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

    let cookie = log_in(&app, &captured).await;
    let resp = app
        .oneshot(get_with_cookie("/api/setup/account-directory", &cookie))
        .await
        .unwrap();
    // OBP-API's directory response is relayed verbatim, routings included.
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let accounts = v["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["account_id"], "settlement-a");
    assert_eq!(
        accounts[0]["account_routings"],
        json!([
            { "scheme": "OBP", "address": "settlement-a" },
            { "scheme": "CARDANO", "address": "addr_test1aaa" },
        ])
    );
}

#[tokio::test]
async fn request_entitlement_files_a_request_and_treats_duplicates_as_ok() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .clone()
        .oneshot(post_json_with_cookie(
            "/api/setup/request-entitlement",
            &cookie,
            json!({ "bank_id": "rt.bank.a", "role": "CanCreateAccount" }),
        ))
        .await
        .unwrap();
    // OBP-API's response is relayed verbatim: status and body.
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    assert_eq!(v["entitlement_request_id"], "er1");
    {
        let c = captured.lock().unwrap();
        assert_eq!(c.entitlement_requests[0]["bank_id"], "rt.bank.a");
        assert_eq!(c.entitlement_requests[0]["role_name"], "CanCreateAccount");
    }

    // The stub refuses this one as a pending duplicate — relayed as-is; the
    // UI (alreadyOk in setup.js) treats the duplicate code as a no-op.
    let resp = app
        .oneshot(post_json_with_cookie(
            "/api/setup/request-entitlement",
            &cookie,
            json!({ "bank_id": "", "role": "CanCreateBank" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("EntitlementRequestAlreadyExists"));
}

#[tokio::test]
async fn apply_posts_the_configured_bodies_and_grants_roles() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);
    let cookie = log_in(&app, &captured).await;

    // Routing scheme: the config body goes to OBP verbatim.
    let resp = app
        .clone()
        .oneshot(post_json_with_cookie(
            "/api/setup/apply",
            &cookie,
            json!({ "id": "scheme:IBAN" }),
        ))
        .await
        .unwrap();
    // OBP-API's 201 and body come back verbatim.
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    assert_eq!(v["scheme"], "IBAN");
    {
        let c = captured.lock().unwrap();
        assert_eq!(c.scheme_posts.len(), 1);
        assert_eq!(c.scheme_posts[0]["scheme"], "IBAN");
        assert_eq!(c.scheme_posts[0]["country"], "INT");
    }

    // Role grant: resolved to the existing user's id, never a creation.
    let resp = app
        .clone()
        .oneshot(post_json_with_cookie(
            "/api/setup/apply",
            &cookie,
            json!({ "id": "grant:rt.node.a@rt.bank.a:CanSettleOpenCorridor" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let v = body_json(resp).await;
    assert_eq!(v["entitlement_id"], "e1");
    {
        let c = captured.lock().unwrap();
        let (user_id, body) = &c.entitlement_posts[0];
        assert_eq!(user_id, "node-a-user-id");
        assert_eq!(body["bank_id"], "rt.bank.a");
        assert_eq!(body["role_name"], "CanSettleOpenCorridor");
    }

    // Account apply: owner resolved from owner_username.
    let resp = app
        .clone()
        .oneshot(post_json_with_cookie(
            "/api/setup/apply",
            &cookie,
            json!({ "id": "account:rt.bank.a/settlement-a" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["account_id"], "settlement-a");
    {
        let c = captured.lock().unwrap();
        let (path, body) = &c.account_puts[0];
        assert_eq!(path, "rt.bank.a/settlement-a");
        assert_eq!(body["user_id"], "node-a-user-id");
        assert_eq!(body["balance"]["currency"], "KES");
        assert_eq!(body["product_code"], "OPEN_CORRIDOR");
        assert_eq!(
            body["account_routings"],
            json!([{ "scheme": "CARDANO", "address": "addr_test1aaa" }])
        );
    }

    // Grant for a non-existent user is refused — no registration path.
    let resp = app
        .oneshot(post_json_with_cookie(
            "/api/setup/apply",
            &cookie,
            json!({ "id": "grant:ghost@rt.bank.a:CanSettleOpenCorridor" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let v = body_json(resp).await;
    assert!(v["message"]
        .as_str()
        .unwrap()
        .contains("provisioned outside this app"));
}

#[tokio::test]
async fn test_account_endpoint_creates_an_account_owned_by_me_by_default() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_setup(&base);
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .oneshot(post_json_with_cookie(
            "/api/setup/test-account",
            &cookie,
            json!({
                "bank_id": "rt.bank.a",
                "label": "Alice",
                "currency": "KES",
                "routing_scheme": "IBAN",
                "routing_address": "KE9300001234567890",
            }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    // OBP-API generates the account id; its response is relayed verbatim.
    assert_eq!(v["account_id"], "generated-account-id");
    let c = captured.lock().unwrap();
    let (bank_id, body) = &c.account_posts[0];
    assert_eq!(bank_id, "rt.bank.a");
    assert_eq!(body["user_id"], "admin-user-id");
    assert_eq!(body["label"], "Alice");
    assert_eq!(
        body["account_routings"],
        json!([{ "scheme": "IBAN", "address": "KE9300001234567890" }])
    );
}

fn items_by_id(v: &Value) -> HashMap<String, Value> {
    v["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| (i["id"].as_str().unwrap().to_string(), i.clone()))
        .collect()
}

#[tokio::test]
async fn status_checks_the_node_consumer_platform_app_scopes_and_view_access() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_config(&base, node_setup_config("obp-bank-node-a"));
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .oneshot(get_with_cookie("/api/setup/status", &cookie))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let by_id = items_by_id(&body_json(resp).await);

    assert_eq!(by_id["consumer:rt.bank.a"]["status"], "ok");
    assert_eq!(by_id["consumer:rt.bank.a"]["section"], "node");
    assert_eq!(by_id["consumer:rt.bank.a"]["detail"], "consumer_id node-a-consumer-id");
    assert_eq!(by_id["platformapp:rt.bank.a"]["status"], "missing");
    assert_eq!(by_id["scope:rt.bank.a:CanAttachOpenCorridorPromise"]["status"], "ok");
    // Held, but at another bank: missing here.
    assert_eq!(by_id["scope:rt.bank.a:CanSettleOpenCorridor"]["status"], "missing");
    assert_eq!(by_id["nodeview:rt.bank.a/settlement-a"]["status"], "ok");
    // Another Consumer's access to the same view does not count.
    assert_eq!(by_id["nodeview:rt.bank.a/settlement-a-ngn"]["status"], "missing");
    assert_eq!(by_id["nodeview:rt.bank.a/settlement-a-usd"]["status"], "missing");
    assert_eq!(
        by_id["nodeview:rt.bank.a/settlement-a-usd"]["detail"],
        "account rt.bank.a/settlement-a-usd not created yet"
    );
    // The roles these checks and applies need are listed under "me".
    for role in [
        "myrole::CanGetConsumers",
        "myrole::CanGetPlatformApps",
        "myrole::CanCreatePlatformApp",
        "myrole::CanGetEntitlementsForAnyUserAtAnyBank",
        "myrole:rt.bank.a:CanCreateScopeAtOneBank",
        "myrole::CanCreateCustomView",
    ] {
        assert!(by_id.contains_key(role), "{role} should be listed");
    }
}

#[tokio::test]
async fn status_without_the_node_consumer_says_how_to_create_it() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_config(&base, node_setup_config("obp-bank-node-ghost"));
    let cookie = log_in(&app, &captured).await;

    let resp = app
        .clone()
        .oneshot(get_with_cookie("/api/setup/status", &cookie))
        .await
        .unwrap();
    let by_id = items_by_id(&body_json(resp).await);
    // "error" items get no Apply: the page can't create the Consumer.
    assert_eq!(by_id["consumer:rt.bank.a"]["status"], "error");
    assert!(by_id["consumer:rt.bank.a"]["detail"]
        .as_str()
        .unwrap()
        .contains("dev/create_consumers.sh"));
    assert_eq!(by_id["platformapp:rt.bank.a"]["status"], "error");
    assert_eq!(by_id["nodeview:rt.bank.a/settlement-a"]["status"], "error");

    // Applying the Consumer item explains instead of creating anything.
    let resp = app
        .oneshot(post_json_with_cookie(
            "/api/setup/apply",
            &cookie,
            json!({ "id": "consumer:rt.bank.a" }),
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    assert!(captured.lock().unwrap().node_posts.is_empty());
}

#[tokio::test]
async fn apply_marks_the_platform_app_adds_scopes_and_grants_the_node_view() {
    let captured = Arc::new(Mutex::new(Captured::default()));
    let base = spawn_obp_stub(captured.clone()).await;
    let app = app_with_config(&base, node_setup_config("obp-bank-node-a"));
    let cookie = log_in(&app, &captured).await;
    let apply = |id: &str| post_json_with_cookie("/api/setup/apply", &cookie, json!({ "id": id }));

    let resp = app.clone().oneshot(apply("platformapp:rt.bank.a")).await.unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    let resp = app
        .clone()
        .oneshot(apply("scope:rt.bank.a:CanSettleOpenCorridor"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);

    // The view already exists (OBP-30266): the apply goes on to the grant.
    let resp = app
        .clone()
        .oneshot(apply("nodeview:rt.bank.a/settlement-a-ngn"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    assert_eq!(body_json(resp).await["consumer_account_access_id"], "caa1");

    // Ids the config does not name are refused, not improvised.
    let resp = app
        .clone()
        .oneshot(apply("scope:rt.bank.a:CanCreateBank"))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);

    let c = captured.lock().unwrap();
    let posts: Vec<(&str, &Value)> = c.node_posts.iter().map(|(p, b)| (p.as_str(), b)).collect();
    assert_eq!(posts[0].0, "platform-apps");
    assert_eq!(posts[0].1["consumer_id"], "node-a-consumer-id");
    assert_eq!(posts[0].1["label"], "OBP Bank Node A (rt.bank.a)");
    assert_eq!(posts[1].0, "scopes:node-a-consumer-id");
    assert_eq!(posts[1].1, &json!({ "bank_id": "rt.bank.a", "role_name": "CanSettleOpenCorridor" }));
    assert_eq!(posts[2].0, "grant-view:settlement-a-ngn");
    assert_eq!(posts[2].1, &json!({ "user_id": "admin-user-id", "view_id": "ManageCustomViews" }));
    assert_eq!(posts[3].0, "create-view:settlement-a-ngn");
    assert_eq!(posts[3].1["name"], "_open-corridor-node");
    assert_eq!(posts[3].1["allowed_actions"], json!(["can_add_transaction_request_to_any_account"]));
    assert_eq!(posts[4].0, "consumer-access:settlement-a-ngn");
    assert_eq!(
        posts[4].1,
        &json!({ "consumer_id": "node-a-consumer-id", "view_id": "_open-corridor-node" })
    );
    assert_eq!(posts.len(), 5);
}
