// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! The app's own root `/alive`, `/health` and `/status`, in the same format as OBP-API's (`AliveCheckRoutes.scala` and `StatusPage.scala`) and the node's (`obp-bank-node/src/rest/status_page.rs`), so the same monitoring works on all three:
//!
//! - `/alive` answers `true`.
//! - `/health` is liveness: `{"status":"ok"}` whenever the process answers. It touches no dependency, and it is this app's own answer, never the node's.
//! - `/status` is readiness: instance details and one check per dependency. It is HTML by default, JSON when the request accepts `application/json`, and 503 when an essential check is not ok.
//!
//! The essential dependency is the app's nodes: every page reads from them, so each configured node must answer its root `/health`. OBP-API is used only by the `/setup` and `/accounts` operator pages, so it is reported as a soft check that does not change the overall status. None of the three needs a login, and every answer tells caches not to store it.

// The probe client is made once and shared.
use std::sync::{Arc, OnceLock};
// The probe timeout, and the time the app started.
use std::time::{Duration, Instant};

// Axum types for the handlers and router.
use axum::extract::State;
// HTTP headers and status codes.
use axum::http::{header, HeaderMap, StatusCode};
// Building responses.
use axum::response::{IntoResponse, Response};
// GET routes.
use axum::routing::get;
// The router these routes live on.
use axum::Router;
// The JSON report is serialised with serde.
use serde::Serialize;

// The configured nodes.
use crate::NodeConfig;

// How long a check waits for an answer before calling it a failure.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);

// What `/status` needs to know about this app and its dependencies.
#[derive(Clone)]
pub struct StatusState(Arc<StatusInner>);

// The shared contents of StatusState.
struct StatusInner {
    // Each configured node, as (name, base URL), in config order.
    nodes: Vec<(String, String)>,
    // OBP-API's base URL, when the setup page is configured.
    obp_api_url: Option<String>,
    // Identifies this run of the app on `/status`; new on every start.
    instance_id: String,
    // When this run started, for the uptime.
    started_at: Instant,
}

// Ways to build a StatusState.
impl StatusState {
    // Build the state from the configured nodes and, when the setup page is configured, OBP-API's base URL.
    pub fn new(nodes: &[NodeConfig], obp_api_url: Option<String>) -> Self {
        // Keep only each node's name and base URL: the bearer token is not needed to probe the node's root `/health`.
        let node_urls = nodes.iter().map(|node_config| (node_config.name.clone(), node_config.base_url.clone())).collect();
        // Wrap everything in an Arc so cloning the state for each request is cheap.
        StatusState(Arc::new(StatusInner {
            // The nodes to check.
            nodes: node_urls,
            // OBP-API to check, if any.
            obp_api_url,
            // A fresh identifier for this run.
            instance_id: uuid::Uuid::new_v4().to_string(),
            // Now.
            started_at: Instant::now(),
        }))
    }
}

// The routes for `/alive`, `/health` and `/status`.
pub fn router(state: StatusState) -> Router {
    // Start an empty router.
    Router::new()
        // `true`, as OBP-API's AliveCheck answers.
        .route("/alive", get(alive))
        // Liveness: the process answers HTTP.
        .route("/health", get(health))
        // Readiness: the checks below.
        .route("/status", get(status))
        // Every handler above can read the state.
        .with_state(state)
}

// The HTTP client for probes, made the first time it is needed.
fn http() -> &'static reqwest::Client {
    // Holds the client once it is made.
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    // Make the client on first use, with the probe timeout.
    CLIENT.get_or_init(|| reqwest::Client::builder().timeout(PROBE_TIMEOUT).build().expect("reqwest client"))
}

// `GET /alive`: `true`, touching nothing.
pub async fn alive() -> Response {
    // The same body as OBP-API's.
    json_response(StatusCode::OK, "true".to_string())
}

// `GET /health`: `{"status":"ok"}`, touching nothing.
pub async fn health() -> Response {
    // The same body as OBP-API's.
    json_response(StatusCode::OK, r#"{"status":"ok"}"#.to_string())
}

// One check: `ok`, `warn`, `fail` or `disabled`, and what was seen.
#[derive(Debug, Serialize)]
pub struct Check {
    // How the check went.
    pub status: &'static str,
    // What was seen, in words.
    pub detail: String,
}

// Short ways to make a Check.
impl Check {
    // A check that passed.
    fn ok(detail: impl Into<String>) -> Self {
        // Status ok, with the detail.
        Self { status: "ok", detail: detail.into() }
    }
    // A check that failed.
    fn fail(detail: impl Into<String>) -> Self {
        // Status fail, with the detail.
        Self { status: "fail", detail: detail.into() }
    }
    // A check switched off by configuration, which is not a fault.
    fn disabled(detail: impl Into<String>) -> Self {
        // Status disabled, with the detail.
        Self { status: "disabled", detail: detail.into() }
    }
}

// The essential checks: any that is not ok makes `/status` answer 503.
#[derive(Debug, Serialize)]
pub struct Checks {
    // Every configured node answers its root `/health`.
    pub nodes: Check,
}

// The whole `/status` report.
#[derive(Debug, Serialize)]
pub struct StatusReport {
    // `ok` when every essential check is ok, otherwise `degraded`.
    pub status: &'static str,
    // Which app this is.
    pub service: &'static str,
    // The crate version.
    pub version: &'static str,
    // This run's identifier.
    pub instance_id: String,
    // The commit the app was built from.
    pub git_commit: &'static str,
    // The branch it was built from.
    pub git_branch: &'static str,
    // Whether the working tree had uncommitted changes at build time.
    pub git_dirty: bool,
    // When it was built.
    pub git_build_time: &'static str,
    // Seconds since this run started.
    pub uptime_seconds: u64,
    // The essential checks.
    pub checks: Checks,
    // Soft: OBP-API, used only by the operator pages. Does not affect the overall status.
    pub obp_api: Check,
}

// `GET /status`: run the checks and answer with HTML or JSON.
pub async fn status(State(state): State<StatusState>, headers: HeaderMap) -> Response {
    // Run every check.
    let report = run_checks(&state).await;
    // 200 when all essential checks are ok, 503 otherwise, as OBP-API does.
    let code = if report.status == "ok" { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    // A caller that asks for JSON gets JSON.
    if prefers_json(&headers) {
        // The report as pretty JSON.
        let body = serde_json::to_string_pretty(&report).unwrap_or_default();
        // Send it, uncached.
        json_response(code, body)
    // Everyone else, such as a browser, gets HTML.
    } else {
        // The report as an HTML page, uncached.
        (code, [(header::CONTENT_TYPE, "text/html; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], html(&report)).into_response()
    }
}

// Run the checks, all at once, and build the report.
pub async fn run_checks(state: &StatusState) -> StatusReport {
    // Check the nodes and OBP-API at the same time.
    let (nodes, obp_api) = tokio::join!(check_nodes(&state.0.nodes), check_obp_api(state.0.obp_api_url.as_deref()));
    // The essential checks.
    let checks = Checks { nodes };
    // `disabled` is a configured choice, not a fault.
    let all_ok = matches!(checks.nodes.status, "ok" | "disabled");
    // Put the report together.
    StatusReport {
        // The overall status.
        status: if all_ok { "ok" } else { "degraded" },
        // This app.
        service: "OBP-Bank-Node-UI",
        // From Cargo.toml.
        version: env!("CARGO_PKG_VERSION"),
        // This run.
        instance_id: state.0.instance_id.clone(),
        // Stamped by build.rs.
        git_commit: env!("OBP_BANK_NODE_UI_GIT_COMMIT"),
        // Stamped by build.rs.
        git_branch: env!("OBP_BANK_NODE_UI_GIT_BRANCH"),
        // Stamped by build.rs as "true" or "false".
        git_dirty: env!("OBP_BANK_NODE_UI_GIT_DIRTY") == "true",
        // Stamped by build.rs.
        git_build_time: env!("OBP_BANK_NODE_UI_BUILD_TIME"),
        // Whole seconds since start.
        uptime_seconds: state.0.started_at.elapsed().as_secs(),
        // The essential checks.
        checks,
        // The soft check.
        obp_api,
    }
}

// Every configured node answers its root `/health`; the check fails when any node does not.
async fn check_nodes(nodes: &[(String, String)]) -> Check {
    // An app with no nodes has nothing to show, which is a configuration fault.
    if nodes.is_empty() {
        // Say so.
        return Check::fail("no nodes are configured");
    }
    // Probe every node at the same time.
    let node_checks = futures_util::future::join_all(nodes.iter().map(|(_, base_url)| check_node(base_url))).await;
    // The check passes only when every node's does.
    let all_ok = node_checks.iter().all(|node_check| node_check.status == "ok");
    // Name each node in the detail.
    let detail = nodes.iter().zip(&node_checks).map(|((node_name, _), node_check)| format!("{node_name}: {}", node_check.detail)).collect::<Vec<_>>().join("; ");
    // One check for all the nodes.
    if all_ok { Check::ok(detail) } else { Check::fail(detail) }
}

// One node answers its root `/health` with a success.
async fn check_node(base_url: &str) -> Check {
    // The node's own liveness endpoint, which touches none of its dependencies.
    let url = format!("{}/health", base_url.trim_end_matches('/'));
    // Ask it.
    match http().get(&url).send().await {
        // It answered with a success.
        Ok(response) if response.status().is_success() => Check::ok(url),
        // It answered, but not with a success.
        Ok(response) => Check::fail(format!("{url} — HTTP {}", response.status().as_u16())),
        // It did not answer.
        Err(error) => Check::fail(format!("{url} — {error}")),
    }
}

// OBP-API answers its v5.1.0 root, when the setup page is configured.
async fn check_obp_api(obp_api_url: Option<&str>) -> Check {
    // Without an `obp_api` block the app never calls OBP-API.
    let Some(base_url) = obp_api_url else {
        // Nothing to check.
        return Check::disabled("no obp_api block in the config, so the setup page is off");
    };
    // OBP-API's root, which needs no login.
    let url = format!("{}/obp/v5.1.0/root", base_url.trim_end_matches('/'));
    // Ask it.
    match http().get(&url).send().await {
        // It answered with a success.
        Ok(response) if response.status().is_success() => Check::ok(url),
        // It answered, but not with a success.
        Ok(response) => Check::fail(format!("{url} — HTTP {}", response.status().as_u16())),
        // It did not answer.
        Err(error) => Check::fail(format!("{url} — {error}")),
    }
}

// Whether the request's Accept header lists `application/json`, as OBP-API's prefersJson does.
fn prefers_json(headers: &HeaderMap) -> bool {
    // Look at every Accept header.
    headers
        // All of them, not only the first.
        .get_all(header::ACCEPT)
        // Each one's value.
        .iter()
        // Skip values that are not readable text.
        .filter_map(|header_value| header_value.to_str().ok())
        // Split each value into its media ranges.
        .flat_map(|header_text| header_text.split(','))
        // Drop any parameters such as `;q=0.9` and compare.
        .any(|media_range| media_range.split(';').next().unwrap_or("").trim() == "application/json")
}

// A JSON answer with the given code, which caches must not store.
fn json_response(code: StatusCode, body: String) -> Response {
    // The body, its type, and no-store.
    (code, [(header::CONTENT_TYPE, "application/json; charset=utf-8"), (header::CACHE_CONTROL, "no-store")], body).into_response()
}

// Make text safe to put in HTML.
fn esc(text: &str) -> String {
    // Replace the characters HTML treats specially.
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

// A check's status in its colour.
fn badge(status: &str) -> String {
    // Green for ok, grey for disabled, orange for warn, red for anything else.
    let color = match status {
        // Fine.
        "ok" => "#2e7d32",
        // Worth a look.
        "warn" => "#ef6c00",
        // Switched off on purpose.
        "disabled" => "#666",
        // A fault.
        _ => "#c62828",
    };
    // The coloured status.
    format!(r#"<span style="color: {color}; font-weight: bold;">{}</span>"#, esc(status))
}

// One table row for one check.
fn check_row(name: &str, check: &Check) -> String {
    // The detail under the status, when there is one.
    let detail = if check.detail.is_empty() { String::new() } else { format!(r#"<div style="color: #666; font-size: 0.85em; margin-top: 4px;">{}</div>"#, esc(&check.detail)) };
    // The row.
    format!("    <tr><th>{name}</th><td>{}{detail}</td></tr>\n", badge(check.status))
}

// The report as an HTML page, laid out like the node's and OBP-API's.
fn html(report: &StatusReport) -> String {
    // Green heading when ok, red otherwise.
    let overall_color = if report.status == "ok" { "#2e7d32" } else { "#c62828" };
    // A warning when the build came from a working tree with uncommitted changes.
    let dirty = if report.git_dirty { r#" <span style="color: #ef6c00; font-weight: bold;">dirty</span><div style="color: #666; font-size: 0.85em; margin-top: 4px;">built from a working tree with uncommitted changes &mdash; this names the last commit, not necessarily what is running</div>"# } else { "" };
    // Fill in the page.
    format!(
        // The page template.
        r#"<!DOCTYPE html>
<html>
<head>
  <title>OBP Bank Node UI Status</title>
  <style>
    body {{ font-family: sans-serif; max-width: 800px; margin: 40px auto; padding: 0 20px; }}
    h1 {{ color: #333; }}
    h2 {{ color: #555; margin-top: 30px; }}
    table {{ border-collapse: collapse; width: 100%; }}
    th, td {{ padding: 6px 12px; text-align: left; border-bottom: 1px solid #eee; vertical-align: top; }}
    th {{ width: 180px; }}
  </style>
</head>
<body>
  <h1>OBP Bank Node UI Status: <span style="color: {overall_color};">{status}</span></h1>

  <h2>Instance</h2>
  <table>
    <tr><th>instance_id</th><td>{instance_id}</td></tr>
    <tr><th>version</th><td>{version}</td></tr>
    <tr><th>git_commit</th><td>{git_commit}{dirty}</td></tr>
    <tr><th>git_branch</th><td>{git_branch}</td></tr>
    <tr><th>git_build_time</th><td>{git_build_time}</td></tr>
    <tr><th>uptime_seconds</th><td>{uptime}</td></tr>
  </table>

  <h2>Checks</h2>
  <table>
{nodes}  </table>

  <h2>OBP-API (soft &mdash; used only by the setup pages, does not affect overall status)</h2>
  <table>
{obp_api}  </table>
</body>
</html>"#,
        // The overall status.
        status = esc(report.status),
        // This run.
        instance_id = esc(&report.instance_id),
        // The crate version.
        version = esc(report.version),
        // The commit.
        git_commit = esc(report.git_commit),
        // The branch.
        git_branch = esc(report.git_branch),
        // The build time.
        git_build_time = esc(report.git_build_time),
        // The uptime.
        uptime = report.uptime_seconds,
        // The nodes row.
        nodes = check_row("nodes", &report.checks.nodes),
        // The OBP-API row.
        obp_api = check_row("obp_api", &report.obp_api),
    )
}
