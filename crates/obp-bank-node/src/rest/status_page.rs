// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

//! Root `/alive`, `/health` and `/status`, in the same format as OBP-API's
//! (`AliveCheckRoutes.scala` and `StatusPage.scala`), so the same monitoring
//! works on both:
//!
//! - `/alive` — `true`.
//! - `/health` — liveness: `{"status":"ok"}` whenever the process answers.
//!   Touches no dependency.
//! - `/status` — readiness: instance details and one check per dependency.
//!   HTML by default, JSON when the request accepts `application/json`, and
//!   503 when any check is not ok.
//!
//! The versioned `/obp-bank-node/v5.1.0/health` and `/status` keep their own
//! shapes: the Bank Node apps read the node's identity and corridor accounts
//! from them.

use std::sync::OnceLock;
use std::time::Duration;

use axum::{
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use serde::Serialize;

use super::BankNodeState;
use crate::outbox::status as outbox_status;

const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Ogmios reports `networkSynchronization` 1.0 once caught up; anything lower
/// means the node's view of the chain is behind (wallet funds may be missing).
const SYNCED: f64 = 0.999;

fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(PROBE_TIMEOUT)
            .build()
            .expect("reqwest client")
    })
}

pub async fn alive() -> Response {
    json_response(StatusCode::OK, "true".to_string())
}

pub async fn health() -> Response {
    json_response(StatusCode::OK, r#"{"status":"ok"}"#.to_string())
}

/// What a `/ready` probe looks at: the stores, OBP-API and the chain. These
/// are the dependencies a request needs right now; Open Corridor being
/// switched on and Interface C having connected are reported by `/status`
/// but do not take the node out of rotation.
#[derive(Debug, Serialize)]
pub struct ReadyChecks {
    pub database: Check,
    pub obp_api: Check,
    pub blockchain: Check,
}

#[derive(Debug, Serialize)]
pub struct ReadyReport {
    pub status: &'static str,
    pub checks: ReadyChecks,
}

/// Readiness for a load balancer or Kubernetes: 200 `ready` when the stores,
/// OBP-API and the chain answer, otherwise 503 `not_ready` with each check.
pub async fn ready(State(state): State<BankNodeState>) -> Response {
    let (database, obp_api, blockchain) = tokio::join!(
        check_database(&state),
        check_obp_api(&state.obp_api_url),
        check_blockchain(state.blockchain_label, state.chain_health_url.as_deref()),
    );
    let checks = ReadyChecks { database, obp_api, blockchain };
    let all_ok = [&checks.database, &checks.obp_api, &checks.blockchain]
        .iter()
        .all(|c| matches!(c.status, "ok" | "disabled"));
    let (code, status) = if all_ok {
        (StatusCode::OK, "ready")
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "not_ready")
    };
    let body = serde_json::to_string_pretty(&ReadyReport { status, checks }).unwrap_or_default();
    json_response(code, body)
}

/// One check: `ok`, `warn`, `fail` or `disabled`, and what was seen.
#[derive(Debug, Serialize)]
pub struct Check {
    pub status: &'static str,
    pub detail: String,
}

impl Check {
    fn ok(detail: impl Into<String>) -> Self {
        Self { status: "ok", detail: detail.into() }
    }
    fn warn(detail: impl Into<String>) -> Self {
        Self { status: "warn", detail: detail.into() }
    }
    fn fail(detail: impl Into<String>) -> Self {
        Self { status: "fail", detail: detail.into() }
    }
}

#[derive(Debug, Serialize)]
pub struct Checks {
    pub database: Check,
    pub obp_api: Check,
    pub open_corridor: Check,
    pub blockchain: Check,
    pub interface_c: Check,
}

impl Checks {
    /// `disabled` is a configured choice, not a fault.
    fn all_ok(&self) -> bool {
        [&self.database, &self.obp_api, &self.open_corridor, &self.blockchain, &self.interface_c]
            .iter()
            .all(|c| matches!(c.status, "ok" | "disabled"))
    }
}

#[derive(Debug, Serialize)]
pub struct StatusReport {
    pub status: &'static str,
    pub service: &'static str,
    pub version: &'static str,
    pub instance_id: String,
    pub bank_id: String,
    pub git_commit: &'static str,
    pub git_branch: &'static str,
    pub git_dirty: bool,
    pub git_build_time: &'static str,
    pub uptime_seconds: u64,
    pub checks: Checks,
    /// Soft: outbox rows per status. Does not affect the overall status, but
    /// rows in EXCEPTION need an operator.
    pub outbox: Check,
    /// Soft: settlements this bank is owed that need an operator — reported
    /// paid but never seen arriving (MISMATCH), or netted long ago with the
    /// debtor yet to pay.
    pub incoming_settlements: Check,
}

/// A NETTED incoming settlement older than this is worth a look: the debtor's
/// node normally pays within minutes of the netting.
const NETTED_STALE_AFTER: Duration = Duration::from_secs(60 * 60);

pub async fn status(State(state): State<BankNodeState>, headers: HeaderMap) -> Response {
    let report = run_checks(&state).await;
    let code = if report.status == "ok" {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    if prefers_json(&headers) {
        let body = serde_json::to_string_pretty(&report).unwrap_or_default();
        json_response(code, body)
    } else {
        (code, [(header::CONTENT_TYPE, "text/html; charset=utf-8")], html(&report)).into_response()
    }
}

pub async fn run_checks(state: &BankNodeState) -> StatusReport {
    let (database, obp_api, open_corridor, blockchain, outbox, incoming_settlements) = tokio::join!(
        check_database(state),
        check_obp_api(&state.obp_api_url),
        check_open_corridor(&state.obp_api_url, &state.bank_id),
        check_blockchain(state.blockchain_label, state.chain_health_url.as_deref()),
        check_outbox(state),
        check_incoming(state),
    );
    let checks = Checks {
        database,
        obp_api,
        open_corridor,
        blockchain,
        interface_c: check_interface_c(state),
    };
    StatusReport {
        status: if checks.all_ok() { "ok" } else { "degraded" },
        service: "OBP-Bank-Node",
        version: env!("CARGO_PKG_VERSION"),
        instance_id: state.instance_id.clone(),
        bank_id: state.bank_id.clone(),
        git_commit: env!("OBP_BANK_NODE_GIT_COMMIT"),
        git_branch: env!("OBP_BANK_NODE_GIT_BRANCH"),
        git_dirty: env!("OBP_BANK_NODE_GIT_DIRTY") == "true",
        git_build_time: env!("OBP_BANK_NODE_BUILD_TIME"),
        uptime_seconds: state.started_at.elapsed().as_secs(),
        checks,
        outbox,
        incoming_settlements,
    }
}

async fn check_incoming(state: &BankNodeState) -> Check {
    let cutoff = (chrono::Utc::now() - chrono::Duration::from_std(NETTED_STALE_AFTER).unwrap_or_default())
        .to_rfc3339();
    match state.settlements.incoming_attention(&cutoff).await {
        Ok(a) if a.mismatch == 0 && a.stale_netted == 0 => {
            match state.settlements.list_incoming(1).await {
                Ok(rows) if rows.is_empty() => Check::ok("no incoming settlements"),
                Ok(_) => Check::ok("none need attention"),
                Err(e) => Check::fail(e.to_string()),
            }
        }
        Ok(a) => {
            let hours = NETTED_STALE_AFTER.as_secs() / 3600;
            Check::warn(format!(
                "MISMATCH {} (reported paid, nothing arrived), NETTED for more than {hours} h {} — \
                 see /obp-bank-node/v5.1.0/incoming-settlements; ids: {}",
                a.mismatch,
                a.stale_netted,
                a.ids.join(", ")
            ))
        }
        Err(e) => Check::fail(e.to_string()),
    }
}

async fn check_database(state: &BankNodeState) -> Check {
    let (outbox, settlements, evidence) = tokio::join!(
        state.outbox.ping(),
        state.settlements.ping(),
        state.evidence.ping(),
    );
    let failed: Vec<String> = [("outbox", outbox), ("settlements", settlements), ("evidence", evidence)]
        .into_iter()
        .filter_map(|(name, r)| r.err().map(|e| format!("{name}: {e}")))
        .collect();
    if failed.is_empty() {
        Check::ok("outbox, settlements and evidence stores answer")
    } else {
        Check::fail(failed.join("; "))
    }
}

async fn check_obp_api(base_url: &str) -> Check {
    let url = format!("{}/obp/v6.0.0/root", base_url.trim_end_matches('/'));
    match http().get(&url).send().await {
        Ok(r) if r.status().is_success() => Check::ok(url),
        Ok(r) => Check::fail(format!("{url} — HTTP {}", r.status().as_u16())),
        Err(e) => Check::fail(format!("{url} — {e}")),
    }
}

/// Whether OBP-API has Open Corridor on: it lists OPEN_CORRIDOR_PROMISE among
/// the bank's transaction request types exactly when `open_corridor_enabled`
/// is true. Without it, OBP-API refuses promises and settlements (OBP-40057).
async fn check_open_corridor(base_url: &str, bank_id: &str) -> Check {
    let url = format!(
        "{}/obp/v5.1.0/banks/{bank_id}/transaction-request-types",
        base_url.trim_end_matches('/')
    );
    let resp = match http().get(&url).send().await {
        Ok(r) => r,
        Err(e) => return Check::fail(format!("{url} — {e}")),
    };
    if !resp.status().is_success() {
        return Check::warn(format!(
            "{url} — HTTP {}: cannot tell whether Open Corridor is on",
            resp.status().as_u16()
        ));
    }
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Check::fail(format!("{url} — unreadable answer: {e}")),
    };
    let listed = body["transaction_request_types"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|t| t["transaction_request_type"] == "OPEN_CORRIDOR_PROMISE");
    if listed {
        Check::ok(format!("OPEN_CORRIDOR_PROMISE is listed ({url})"))
    } else {
        Check::fail(format!(
            "OBP-API does not list OPEN_CORRIDOR_PROMISE, so it refuses promises and settlements: \
             set open_corridor_enabled=true in its props and restart it ({url})"
        ))
    }
}

/// For Cardano, Ogmios's `/health`: reachable, and how far the node has synced.
async fn check_blockchain(label: &str, health_url: Option<&str>) -> Check {
    let Some(url) = health_url else {
        return Check::ok(format!("{label} backend, no chain to check"));
    };
    let resp = match http().get(url).send().await {
        Ok(r) => r,
        Err(e) => return Check::fail(format!("{url} — {e}")),
    };
    let body: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return Check::fail(format!("{url} — unreadable answer: {e}")),
    };
    let sync = body["networkSynchronization"].as_f64().unwrap_or(0.0);
    let network = body["network"].as_str().unwrap_or("?");
    let era = body["currentEra"].as_str().unwrap_or("?");
    let height = body["lastKnownTip"]["height"].as_u64().unwrap_or(0);
    let seen = format!(
        "{label} {network}, {era} era, block {height}, synced {:.2}% ({url})",
        sync * 100.0
    );
    if body["connectionStatus"].as_str() != Some("connected") {
        Check::fail(format!("Ogmios is not connected to cardano-node — {seen}"))
    } else if sync < SYNCED {
        Check::warn(format!(
            "still syncing: promises and settlements cannot settle until it reaches 100% — {seen}"
        ))
    } else {
        Check::ok(seen)
    }
}

fn check_interface_c(state: &BankNodeState) -> Check {
    match state.interface_c.snapshot() {
        ("connected", _) => Check::ok("consuming from RabbitMQ"),
        ("disabled", _) => Check {
            status: "disabled",
            detail: "rabbitmq.enabled is false".into(),
        },
        (s, detail) => Check::fail(match detail {
            Some(d) => format!("{s} — {d}"),
            None => s.to_string(),
        }),
    }
}

async fn check_outbox(state: &BankNodeState) -> Check {
    match state.outbox.count_by_status().await {
        Ok(counts) => {
            let total: i64 = counts.iter().map(|(_, n)| n).sum();
            let summary = if counts.is_empty() {
                "no transaction requests".to_string()
            } else {
                counts
                    .iter()
                    .map(|(s, n)| format!("{s} {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let exceptions = counts
                .iter()
                .find(|(s, _)| s == outbox_status::EXCEPTION)
                .map_or(0, |(_, n)| *n);
            if exceptions > 0 {
                Check::warn(format!(
                    "{summary} ({total} in all) — rows in EXCEPTION are not retried and need an operator"
                ))
            } else {
                Check::ok(summary)
            }
        }
        Err(e) => Check::fail(e.to_string()),
    }
}

fn prefers_json(headers: &HeaderMap) -> bool {
    headers
        .get_all(header::ACCEPT)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .any(|m| m.split(';').next().unwrap_or("").trim() == "application/json")
}

fn json_response(code: StatusCode, body: String) -> Response {
    (code, [(header::CONTENT_TYPE, "application/json; charset=utf-8")], body).into_response()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn badge(status: &str) -> String {
    let color = match status {
        "ok" => "#2e7d32",
        "warn" => "#ef6c00",
        "disabled" => "#666",
        _ => "#c62828",
    };
    format!(r#"<span style="color: {color}; font-weight: bold;">{}</span>"#, esc(status))
}

fn check_row(name: &str, c: &Check) -> String {
    let detail = if c.detail.is_empty() {
        String::new()
    } else {
        format!(
            r#"<div style="color: #666; font-size: 0.85em; margin-top: 4px;">{}</div>"#,
            esc(&c.detail)
        )
    };
    format!("    <tr><th>{name}</th><td>{}{detail}</td></tr>\n", badge(&c.status))
}

fn html(r: &StatusReport) -> String {
    let overall_color = if r.status == "ok" { "#2e7d32" } else { "#c62828" };
    let dirty = if r.git_dirty {
        r#" <span style="color: #ef6c00; font-weight: bold;">dirty</span><div style="color: #666; font-size: 0.85em; margin-top: 4px;">built from a working tree with uncommitted changes &mdash; this names the last commit, not necessarily what is running</div>"#
    } else {
        ""
    };
    format!(
        r#"<!DOCTYPE html>
<html>
<head>
  <title>OBP Bank Node - Status</title>
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
  <h1>OBP Bank Node Status: <span style="color: {overall_color};">{status}</span></h1>

  <h2>Instance</h2>
  <table>
    <tr><th>instance_id</th><td>{instance_id}</td></tr>
    <tr><th>bank_id</th><td>{bank_id}</td></tr>
    <tr><th>version</th><td>{version}</td></tr>
    <tr><th>git_commit</th><td>{git_commit}{dirty}</td></tr>
    <tr><th>git_branch</th><td>{git_branch}</td></tr>
    <tr><th>git_build_time</th><td>{git_build_time}</td></tr>
    <tr><th>uptime_seconds</th><td>{uptime}</td></tr>
  </table>

  <h2>Checks</h2>
  <table>
{database}{obp_api}{open_corridor}{blockchain}{interface_c}  </table>

  <h2>Outbox and incoming settlements (soft &mdash; do not affect overall status)</h2>
  <table>
{outbox}{incoming}  </table>
</body>
</html>"#,
        status = esc(r.status),
        instance_id = esc(&r.instance_id),
        bank_id = esc(&r.bank_id),
        version = esc(r.version),
        git_commit = esc(r.git_commit),
        git_branch = esc(r.git_branch),
        git_build_time = esc(r.git_build_time),
        uptime = r.uptime_seconds,
        database = check_row("database", &r.checks.database),
        obp_api = check_row("obp_api", &r.checks.obp_api),
        open_corridor = check_row("open_corridor", &r.checks.open_corridor),
        blockchain = check_row("blockchain", &r.checks.blockchain),
        interface_c = check_row("interface_c", &r.checks.interface_c),
        outbox = check_row("transaction requests", &r.outbox),
        incoming = check_row("incoming settlements", &r.incoming_settlements),
    )
}
