// Copyright (C) 2025-2026 TESOBE GmbH
// SPDX-License-Identifier: AGPL-3.0-or-later
// Licensed under the GNU Affero General Public License v3 or later: see the LICENSE and NOTICE files.

// OBP Bank Node UI — vanilla JS, no build step.
// The app displays; the nodes decide. Everything below is fetch + render.

const POLL_MS = 3000;
const CARDANOSCAN = "https://preprod.cardanoscan.io/transaction/";

let NODES = []; // [{name}] from /api/nodes, config order

const $ = (sel) => document.querySelector(sel);

async function getJson(url) {
  const resp = await fetch(url);
  const body = await resp.json().catch(() => null);
  return { ok: resp.ok, status: resp.status, body };
}

async function postJson(url, payload) {
  const resp = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(payload),
  });
  const body = await resp.json().catch(() => null);
  return { ok: resp.ok, status: resp.status, body };
}

const nodeUrl = (node, path) => `/api/nodes/${encodeURIComponent(node)}/${path}`;

function short(id, n = 12) {
  if (!id) return "";
  return id.length <= n ? id : id.slice(0, n) + "…";
}

function txLink(txId) {
  if (!txId) return "";
  return `<a href="${CARDANOSCAN}${txId}" target="_blank" rel="noopener" title="${txId}">${short(txId)}</a>`;
}

function chip(status) {
  const cls = (status || "unknown").toLowerCase().replace(/[^a-z]/g, "-");
  return `<span class="chip chip-${cls}">${status || "?"}</span>`;
}

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

// A container's `data-limit` caps how many rows its tables show (the index
// shows the most recent few; the dedicated pages omit the attribute and show
// everything). Rows arrive from the node APIs newest first.
function rowLimit(el) {
  const n = Number(el.dataset.limit);
  return Number.isFinite(n) && n > 0 ? n : Infinity;
}

function moreLink(total, shown, href, what) {
  if (total <= shown) return "";
  return `<p class="more">showing ${shown} of ${total} · <a href="${href}">all ${what}</a></p>`;
}

// ---- header: node health -------------------------------------------------

async function renderHealth() {
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "health"));
    const up = r.ok && r.body && r.body.status === "healthy";
    const chain = up ? ` · ${esc(r.body.blockchain)}` : "";
    // "disabled" is a deliberate config; anything else non-connected means the
    // node cannot receive settlement instructions or credit notifications.
    const ifc = up && r.body.interface_c && !["connected", "disabled"].includes(r.body.interface_c);
    if (ifc) {
      return `<span class="node-chip warn" title="${esc(r.body.interface_c_detail || r.body.interface_c)}">
        ${esc(nodeName(n.name))} · up${chain} · AMQP ${esc(r.body.interface_c)}</span>`;
    }
    return `<span class="node-chip ${up ? "up" : "down"}">${esc(nodeName(n.name))} · ${up ? "up" : "down"}${chain}</span>`;
  }));
  // Group the node chips by the OBP host each node is on.
  // A hub node is on several OBP hosts, so it appears in each group.
  const groups = new Map();
  NODES.forEach((n, i) => {
    const names = hostNames(n.name);
    for (const name of names.length ? names : ["?"]) {
      if (!groups.has(name)) groups.set(name, []);
      groups.get(name).push(parts[i]);
    }
  });
  $("#node-health").innerHTML = [...groups].map(([name, chips]) =>
    `<span class="host-group"><span class="host-name" title="OBP host">${esc(name)}</span>${chips.join("")}</span>`,
  ).join("");
  renderSendNodeAccount();
}

// Per-node /status (configuration detail: corridor_accounts map etc.).
// Fetched once per node — /status only changes on a node restart.
const NODE_STATUS = {};

async function fetchStatus(name) {
  if (!NODE_STATUS[name]) {
    const r = await getJson(nodeUrl(name, "status"));
    if (r.ok && r.body) NODE_STATUS[name] = r.body;
  }
  return NODE_STATUS[name];
}

// A node's own name, from its /status (the node's config `name`). This UI's config also names each node, and uses that name in its URLs; it is shown until the node's /status has answered.
function nodeName(uiName) {
  return NODE_STATUS[uiName]?.name || uiName;
}

// The OBP hosts a node is on, from its /status. One today;
// a hub node lists several.
function nodeHosts(name) {
  return NODE_STATUS[name]?.hosts || [];
}

function hostNames(name) {
  return nodeHosts(name).map((host) => host.host);
}

// The node's bank on an OBP host (the selected one unless named): its entry in the node's /status `hosts`, or null when the node is not on that OBP host.
function nodeHostEntry(node, host = CURRENT_HOST) {
  return nodeHosts(node).find((entry) => entry.host === host) || null;
}

// A URL for the node's bank on the selected OBP host: /hosts/HOST/banks/BANK_ID/PATH, mirroring OBP-API's own URLs. Null when the node is not on the selected OBP host.
function nodeBankUrl(node, path) {
  const entry = nodeHostEntry(node);
  if (!entry) return null;
  return nodeUrl(node, `hosts/${encodeURIComponent(entry.host)}/banks/${encodeURIComponent(entry.bank_id)}/${path}`);
}

// GET a URL from nodeBankUrl; a missing URL (node not on the selected OBP host) reads as a failed request.
async function getBankJson(url) {
  return url ? getJson(url) : { ok: false, status: 404, body: null };
}

// A node's heading in the tables: its name and the OBP host(s) it is on.
function nodeTitle(name) {
  const names = hostNames(name);
  return names.length
    ? `${esc(nodeName(name))} <span class="dim">· on ${esc(names.join(", "))}</span>`
    : esc(nodeName(name));
}

// ---- OBP host tabs ---------------------------------------------------------

// The UI shows one OBP host at a time: the
// tables, the Send and Settle forms and the latest settlement cover the nodes
// on the selected OBP host. The header still shows every node. The choice is
// kept in the browser, so it stays the same from page to page.
const HOST_KEY = "bank-node-ui:host";
// The tab for nodes whose /status has not answered, so their OBP host is not known.
const UNKNOWN_HOST = "?";
// A link can open a given OBP host's tab with ?host=HOST.
let CURRENT_HOST = new URLSearchParams(location.search).get("host")
  || localStorage.getItem(HOST_KEY) || "";

// The OBP hosts a node is on, or the unknown tab when its /status has not answered.
function nodeHostNames(name) {
  const names = hostNames(name);
  return names.length ? names : [UNKNOWN_HOST];
}

// Every OBP host any node is on, in the order of the configured nodes.
function allHosts() {
  return [...new Set(NODES.flatMap((n) => nodeHostNames(n.name)))];
}

// The OBP-API URL on an OBP host, from the first node on it.
function hostUrl(hostName) {
  for (const n of NODES) {
    const host = nodeHosts(n.name).find((h) => h.host === hostName);
    if (host) return host.obp_api_url;
  }
  return "";
}

// The nodes on the selected OBP host. A hub node is on several, so it is on several tabs.
function visibleNodes() {
  return NODES.filter((n) => nodeHostNames(n.name).includes(CURRENT_HOST));
}

// Draw the tab bar under the header, and keep the selection on an OBP host that
// exists. Returns true when the selection changed, so the caller can redraw.
function renderHostTabs() {
  const header = $("header");
  // The Network page shows every OBP host at once, so it has no tabs.
  if (!header || $("#network-view")) return false;
  const hosts = allHosts();
  const before = CURRENT_HOST;
  if (!hosts.includes(CURRENT_HOST)) CURRENT_HOST = hosts[0] || "";
  let nav = $("#host-tabs");
  if (!nav) {
    nav = document.createElement("nav");
    nav.id = "host-tabs";
    nav.className = "host-tabs";
    nav.setAttribute("aria-label", "OBP hosts");
    header.after(nav);
  }
  nav.innerHTML = hosts.map((name) => {
    const url = hostUrl(name);
    const title = name === UNKNOWN_HOST
      ? "nodes whose OBP host is not known yet: their /status has not answered"
      : `OBP host · OBP-API ${url}`;
    const active = name === CURRENT_HOST;
    return `<button type="button" class="host-tab${active ? " active" : ""}" data-host="${esc(name)}"
      title="${esc(title)}" aria-pressed="${active}">${esc(name)}</button>`;
  }).join("");
  return CURRENT_HOST !== before;
}

// Show another OBP host: redraw the tabs, the node pickers and the tables.
async function selectHost(name) {
  CURRENT_HOST = name;
  localStorage.setItem(HOST_KEY, name);
  renderHostTabs();
  await refillForms();
  await refresh();
}

// Read-only line under the Sending node select: the corridor account the
// promise TR for the entered currency will be booked on — or a warning when
// the node has no account for that currency (the node would refuse with
// OBP-BANK-NODE-ROUTING-001).
async function renderSendNodeAccount() {
  const el = $("#send-node-account");
  if (!el) return;
  const node = $("#send-node")?.value;
  if (!node) { el.textContent = ""; return; }
  await fetchStatus(node);
  // The node's bank on the selected OBP host.
  const entry = nodeHostEntry(node);
  if (!entry) { el.textContent = ""; return; }
  const currency = ($("#send-form")?.elements.namedItem("currency")?.value || "")
    .trim().toUpperCase();
  const map = entry.corridor_accounts || {};
  if (Object.keys(map).length && currency) {
    el.textContent = map[currency]
      ? `corridor account: ${bankLabel(entry.bank_id, node)} / ${map[currency]} · on ${entry.host}`
      : `no corridor account for ${currency} — the node will refuse this send`;
    return;
  }
  el.textContent = `corridor account: ${bankLabel(entry.bank_id, node)} / ${entry.account_id} · on ${entry.host}`;
}

// ---- the other bank: picked from getBanks ---------------------------------

// "node|OBP host" -> the banks on that OBP host, as the node relays its getBanks
// (GET /obp-bank-node/v5.1.0/hosts/HOST/banks); null when the node could not list them.
const BANKS = {};

// The cache key for a node's banks on an OBP host (the selected one unless named).
const banksKey = (node, host = CURRENT_HOST) => `${node}|${host}`;

// A node's banks on the selected OBP host.
async function fetchBanks(node) {
  return fetchBanksOn(node, CURRENT_HOST);
}

// A node's banks on an OBP host. null when the node is not on that OBP host or could not list them.
async function fetchBanksOn(node, host) {
  // Only for an OBP host the node is on.
  const entry = nodeHostEntry(node, host);
  if (!entry) return null;
  // Ask again when an earlier try failed (null), for example because the node was down.
  if (!BANKS[banksKey(node, host)]) {
    const r = await getJson(nodeUrl(node, `hosts/${encodeURIComponent(entry.host)}/banks`));
    BANKS[banksKey(node, host)] = r.ok && Array.isArray(r.body?.banks) ? r.body.banks : null;
  }
  return BANKS[banksKey(node, host)];
}

// The bank attribute the setup page sets on each Open Corridor bank. getBanks
// lists every bank on the OBP host; only the ones carrying this are in the corridor.
const PARTICIPANT_ATTRIBUTE = "OPEN_CORRIDOR_PARTICIPANT";

function isParticipant(bank) {
  return (bank.attributes || []).some((attribute) =>
    attribute.name === PARTICIPANT_ATTRIBUTE && String(attribute.value).toLowerCase() === "true");
}

// The banks a node can pay: the OBP host's Open Corridor participants, except
// its own bank. null when the list could not be fetched.
async function otherBanks(node) {
  await fetchStatus(node);
  // The node's own bank on the selected OBP host.
  const ownBankId = nodeHostEntry(node)?.bank_id;
  const banks = await fetchBanks(node);
  return banks ? banks.filter((bank) => bank.bank_id !== ownBankId && isParticipant(bank)) : null;
}

// A bank's BIC: the address of its BIC bank routing. "" when it has none.
function bankBic(bank) {
  const routing = (bank.bank_routings || []).find((r) => String(r.scheme).toUpperCase() === "BIC");
  return routing?.address || "";
}

// A bank_id with its BIC, for example "rt.bank.b4 · BIC DEMBZZ22", as plain text.
// A bank_id is local to one OBP host, so the bank is looked up in the banks
// of the node the row belongs to. Just the bank_id when its BIC is not known.
function bankLabel(bankId, node) {
  if (!bankId) return "";
  const bank = (BANKS[banksKey(node)] || []).find((b) => b.bank_id === bankId);
  const bic = bank ? bankBic(bank) : "";
  return bic ? `${bankId} · BIC ${bic}` : bankId;
}

function bankOption(bank) {
  const routings = (bank.bank_routings || [])
    .map((routing) => `${routing.scheme} ${routing.address}`).join(", ");
  const name = bank.full_name && bank.full_name !== bank.bank_id ? `${bank.full_name} (${bank.bank_id})` : bank.bank_id;
  return `<option value="${esc(bank.bank_id)}">${esc(routings ? `${name} · ${routings}` : name)}</option>`;
}

// Fill a bank picker with the node's other banks. Keeps `wantedBankId` selected
// when it is in the list, else selects the first. Returns the selected bank id,
// or "" when there is nothing to pick.
async function fillBankSelect(select, node, wantedBankId) {
  if (!select || !node) return "";
  const banks = await otherBanks(node);
  if (!banks || banks.length === 0) {
    const why = banks ? "no other Open Corridor banks on this OBP host" : "the node could not list the banks";
    select.innerHTML = `<option value="">${why}</option>`;
    select.disabled = true;
    return "";
  }
  select.disabled = false;
  select.innerHTML = banks.map(bankOption).join("");
  const note = select.parentElement?.querySelector(".fnote");
  const names = hostNames(node);
  if (note && names.length) note.textContent = `Open Corridor banks on ${names.join(", ")} (getBanks)`;
  if (banks.some((bank) => bank.bank_id === wantedBankId)) select.value = wantedBankId;
  return select.value;
}

// The account the beneficiary fields default to: the corridor account of a
// configured node that serves the picked bank (the localhost demo runs both).
// A real deployment fronts one node, so the account stays as typed.
async function accountAtBank(bankId) {
  for (const n of visibleNodes()) {
    await fetchStatus(n.name);
    // The node's bank on the selected OBP host.
    const entry = nodeHostEntry(n.name);
    if (entry?.bank_id === bankId && entry.account_id) return entry.account_id;
  }
  return null;
}

// Point the Send form's beneficiary bank routing at the picked bank.
async function applySendBank(bankId) {
  if (!bankId) return;
  const send = $("#send-form").elements;
  send.namedItem("other_bank_scheme").value = "OBP";
  send.namedItem("other_bank").value = bankId;
  send.namedItem("beneficiary_name").value = `Beneficiary at ${bankId}`;
  const accountId = await accountAtBank(bankId);
  if (accountId) send.namedItem("other_account").value = accountId;
}

async function fillSendBanks() {
  const select = $("#send-other-bank");
  if (!select) return;
  const current = $("#send-form").elements.namedItem("other_bank").value.trim();
  const picked = await fillBankSelect(select, $("#send-node").value, current);
  if (picked && picked !== current) await applySendBank(picked);
}

async function fillSettleBanks() {
  const select = $("#settle-other-bank");
  if (!select) return;
  await fillBankSelect(select, $("#settle-node").value, select.value || $("#send-other-bank")?.value);
}

// ---- step 2: promises (outbox tables) ------------------------------------

let TRS = {}; // node name -> array of transaction request status rows

async function fetchTrs() {
  TRS = {};
  for (const n of visibleNodes()) {
    const r = await getBankJson(nodeBankUrl(n.name, "transaction-requests"));
    TRS[n.name] = r.ok && Array.isArray(r.body) ? r.body : [];
  }
}

// What a row is waiting for, or why it stopped: the node's last error while it
// retries, the reason for EXCEPTION, else what the next step is.
const TR_NEXT = {
  INITIATED: "submitting to OBP-API",
  SUBMITTED: "writing the promise on-chain",
  PROMISE_WRITTEN: "reporting the promise to OBP-API",
  REPORTED: "awaiting settlement",
};

function trDetails(tr) {
  if (tr.status === "EXCEPTION") return tr.exception_reason || "";
  if (tr.last_error) {
    const tries = tr.attempt_count === 1 ? "1 attempt" : `${tr.attempt_count} attempts`;
    return `retrying (${tries}): ${tr.last_error}`;
  }
  if (tr.status === "REPORTED" && tr.settlement_id) return "settled";
  return TR_NEXT[tr.status] || "";
}

function renderTrTables() {
  const el = $("#tr-tables");
  if (!el) return;
  const html = visibleNodes().map((n) => {
    const rows = TRS[n.name] || [];
    if (rows.length === 0) {
      return `<h3>${nodeTitle(n.name)}</h3><p class="empty">no outbound transaction requests</p>`;
    }
    const shown = rows.slice(0, rowLimit(el));
    const body = shown.map((tr) => `
      <tr>
        <td class="id-full">${esc(tr.transaction_request_id)}</td>
        <td>${chip(tr.status)}</td>
        <td class="num">${esc(tr.value ? `${tr.value.amount} ${tr.value.currency}` : "")}</td>
        <td>${esc(bankLabel(tr.other_bank_id, n.name))}</td>
        <td>${tr.promise_id ? txLink(tr.promise_id) : ""}</td>
        <td class="id-full">${esc(tr.settlement_id || "")}</td>
        <td>${esc(tr.settled_at ? tr.settled_at.slice(0, 19) : "")}</td>
        <td class="details">${esc(trDetails(tr))}</td>
      </tr>`).join("");
    return `<h3>${nodeTitle(n.name)}</h3>
      <table>
        <thead><tr><th>transaction request id</th><th>status</th><th>value</th><th>to bank</th>
          <th>promise tx</th><th>settlement</th><th>settled at</th><th>details</th></tr></thead>
        <tbody>${body}</tbody>
      </table>
      ${moreLink(rows.length, shown.length, "/promises", "promises")}`;
  }).join("");
  el.innerHTML = html;
}

// ---- step 3: position (this bank's outbound exposure) --------------------

const IN_CORRIDOR = new Set(["SUBMITTED", "PROMISE_WRITTEN", "REPORTED"]);

function outboundExposure(rows) {
  // "other_bank|currency" -> { other, currency, total, count } over unsettled
  // in-corridor outbound legs. A node only knows its own outbound side; the
  // authoritative bilateral net is OBP-API's, at settle time.
  const acc = {};
  for (const tr of rows) {
    if (!IN_CORRIDOR.has(tr.status) || tr.settlement_id || !tr.value) continue;
    const amount = Number(tr.value.amount);
    if (!Number.isFinite(amount)) continue;
    const key = `${tr.other_bank_id || "?"}|${tr.value.currency}`;
    acc[key] = acc[key] || {
      other: tr.other_bank_id || "?", currency: tr.value.currency, total: 0, count: 0,
    };
    acc[key].total += amount;
    acc[key].count += 1;
  }
  return Object.values(acc);
}

function renderPosition() {
  const el = $("#position-view");
  if (!el) return;
  const html = visibleNodes().map((n) => {
    const corridors = outboundExposure(TRS[n.name] || []);
    if (corridors.length === 0) {
      return `<h3>${nodeTitle(n.name)}</h3><p class="empty">no unsettled outbound corridor legs</p>`;
    }
    const rows = corridors.map((c) => `<tr>
      <td>${esc(c.other === "?" ? "?" : bankLabel(c.other, n.name))}</td>
      <td>${esc(c.currency)}</td>
      <td class="num net">${c.total.toFixed(2)}</td>
      <td class="num">${c.count}</td>
    </tr>`).join("");
    return `<h3>${nodeTitle(n.name)}</h3>
      <table>
        <thead><tr><th>other bank</th><th>currency</th>
          <th>unsettled outbound</th><th>promises</th></tr></thead>
        <tbody>${rows}</tbody>
      </table>`;
  }).join("");
  el.innerHTML = html;
}

// ---- step 4: settlements -------------------------------------------------

// Minor units (assumed exponent 2) -> major, e.g. 175500 -> "1755.00".
function fmtMinor(minor) {
  const n = Number(minor);
  return Number.isFinite(n) ? (n / 100).toFixed(2) : String(minor ?? "");
}

// The rail reports on-chain base units; ADA has 6 decimals (lovelace).
// Settle-time rate cell: "1 ADA = 79.98 KES" plus source · quote-time.
function fmtFx(s) {
  if (!s.fx_minor_per_whole_asset) return "";
  const rate = (Number(s.fx_minor_per_whole_asset) / 100).toFixed(2);
  const when = s.fx_as_of ? s.fx_as_of.slice(11, 19) : "";
  return `<span class="num">1 ${esc(s.asset || "")} = ${esc(rate)} ${esc(s.currency)}</span>
    <div class="dim">${esc(s.fx_source || "")}${when ? ` · ${esc(when)}Z` : ""}</div>`;
}

function fmtRail(amount, asset) {
  if (!amount) return "";
  const n = Number(amount);
  if (asset === "ADA" && Number.isFinite(n)) return `${(n / 1_000_000).toFixed(6)} ADA`;
  return `${amount} ${asset || ""}`;
}

// A retryable ERROR is temporary: the attempt provably never reached the chain,
// OBP-API resends the instruction, and the node tries again (often ending
// FINAL). Only a sticky ERROR needs someone to reconcile it.
function settlementStatus(s) {
  return s.status === "ERROR" && s.retryable ? "RETRYING" : s.status;
}

function settlementError(s) {
  if (!s.error_reason) return "";
  if (s.status !== "ERROR") return s.error_reason;
  return s.retryable
    ? `temporary, OBP-API resends the instruction and this node tries again: ${s.error_reason}`
    : `needs reconciling, the payment may have reached the chain: ${s.error_reason}`;
}

// Settlements a node receives as creditor: NETTED when OBP-API's advice
// arrives (before the debtor pays), PAID once the debtor reports a tx,
// RECEIVED once the node has seen the payment reach its own address.
function incomingTable(n, rows) {
  if (rows.length === 0) return "";
  const body = rows.map((s) => `
      <tr>
        <td class="id-full">${esc(s.settlement_id)}</td>
        <td>${chip(s.status)}</td>
        <td>${esc(bankLabel(s.debtor_bank_id, n.name))}</td>
        <td class="num">${esc(s.net_amount || "")} ${esc(s.currency || "")}</td>
        <td class="num">${esc(fmtRail(s.received_amount || s.asset_amount, s.asset))}</td>
        <td>${txLink(s.tx_id)}</td>
        <td class="details">${esc(s.detail || "")}</td>
      </tr>`).join("");
  return `<h4>${nodeTitle(n.name)} — incoming (as creditor)</h4>
      <table>
        <thead><tr><th>settlement id</th><th>status</th><th>from bank</th><th>net</th>
          <th>rail amount</th><th>tx</th><th>details</th></tr></thead>
        <tbody>${body}</tbody>
      </table>`;
}

// Each node's outgoing settlements (it pays) and incoming ones (it is paid),
// fetched once per refresh for both the tables and the latest-settlement panel.
let SETTLEMENTS = {}; // node name -> { out: [...], in: [...] }

async function fetchSettlements() {
  const entries = await Promise.all(visibleNodes().map(async (n) => {
    const [r, inc] = await Promise.all([
      getBankJson(nodeBankUrl(n.name, "settlements")),
      getBankJson(nodeBankUrl(n.name, "incoming-settlements")),
    ]);
    const list = (x) => (x.ok && Array.isArray(x.body) ? x.body : []);
    return [n.name, { out: list(r), in: list(inc) }];
  }));
  SETTLEMENTS = Object.fromEntries(entries);
}

// The newest settlement across all nodes, followed end to end: the paying
// node's leg (SUBMITTED → FINAL) and the receiving node's check
// (NETTED → PAID → RECEIVED).
function renderLatestSettlement() {
  const el = $("#latest-settlement");
  if (!el) return;
  const byId = {};
  for (const [node, s] of Object.entries(SETTLEMENTS)) {
    for (const o of s.out) {
      const id = o.settlement_id || o.idempotency_key;
      byId[id] = { ...byId[id], id, out: o, payer: node, at: o.created_at };
    }
    for (const i of s.in) {
      const e = byId[i.settlement_id] || { id: i.settlement_id };
      byId[i.settlement_id] = { ...e, in: i, payee: node, at: e.at || i.received_at };
    }
  }
  const latest = Object.values(byId).sort((a, b) => (b.at || "").localeCompare(a.at || ""))[0];
  if (!latest) {
    el.innerHTML = `<p class="empty">no settlements yet</p>`;
    return;
  }
  const o = latest.out;
  const i = latest.in;
  const net = o ? `${fmtMinor(o.net_amount_minor)} ${o.currency}` : `${i.net_amount || ""} ${i.currency || ""}`;
  const rail = o ? fmtRail(o.asset_amount, o.asset) : fmtRail(i.received_amount || i.asset_amount, i.asset);
  const from = o ? nodeName(latest.payer) : (bankLabel(i.debtor_bank_id, latest.payee) || "?");
  const to = i ? nodeName(latest.payee) : "";
  const payLeg = o
    ? `${chip(settlementStatus(o))} depth ${o.depth}/${o.finality_depth} ${txLink(o.tx_id)}
       <div class="details">${esc(settlementError(o))}</div>`
    : `<span class="dim">not paid from a node in this app</span>`;
  const receiveLeg = i
    ? `${chip(i.status)} <div class="details">${esc(i.detail || "")}</div>`
    : `<span class="dim">no node in this app has received the advice yet</span>`;
  el.innerHTML = `
    <p class="latest-head"><span class="id-full">${esc(latest.id)}</span> ·
      <strong>${esc(net)}</strong>${rail ? ` · ${esc(rail)}` : ""} ·
      ${esc((latest.at || "").slice(0, 19).replace("T", " "))}Z</p>
    <table>
      <tbody>
        <tr><th>paid by ${esc(from)}</th><td>${payLeg}</td></tr>
        <tr><th>received by ${esc(to || "creditor")}</th><td>${receiveLeg}</td></tr>
      </tbody>
    </table>`;
}

async function renderSettlements() {
  const el = $("#settlement-tables");
  if (!el) return;
  const parts = visibleNodes().map((n) => {
    const data = SETTLEMENTS[n.name] || { out: [], in: [] };
    const rows = data.out;
    const incoming = incomingTable(n, data.in.slice(0, rowLimit(el)));
    if (rows.length === 0) {
      return `<h3>${nodeTitle(n.name)}</h3><p class="empty">no outgoing settlements</p>${incoming}`;
    }
    const shown = rows.slice(0, rowLimit(el));
    const body = shown.map((s) => `
      <tr>
        <td class="id-full" title="${esc(s.idempotency_key)}">${esc(s.settlement_id || s.idempotency_key)}${
          s.purpose === "PLATFORM_FEE" ? `<div class="dim">platform fee</div>` : ""}</td>
        <td>${chip(settlementStatus(s))}</td>
        <td class="num">${esc(fmtMinor(s.net_amount_minor))} ${esc(s.currency)}</td>
        <td class="num">${esc(fmtRail(s.asset_amount, s.asset))}</td>
        <td>${fmtFx(s)}</td>
        <td class="num">${s.depth}/${s.finality_depth}</td>
        <td>${txLink(s.tx_id)}</td>
        <td class="details">${esc(settlementError(s))}</td>
        <td><button class="small corridor-btn" data-node="${esc(n.name)}"
              data-key="${esc(s.settlement_id || s.idempotency_key)}"
              title="OBP-API's authoritative settlement view — covered promises from both banks; also reconciles this node's linkage">corridor view</button></td>
      </tr>`).join("");
    return `<h3>${nodeTitle(n.name)}</h3>
      <table>
        <thead><tr><th>settlement id</th><th>status</th><th>net</th><th>rail amount</th>
          <th>rate</th><th>depth</th><th>tx</th><th>error</th><th></th></tr></thead>
        <tbody>${body}</tbody>
      </table>
      ${moreLink(rows.length, shown.length, "/settlements", "settlements")}${incoming}`;
  });
  el.innerHTML = parts.join("");
}

async function showCorridor(node, key) {
  const view = $("#corridor-view");
  view.hidden = false;
  view.textContent = "loading corridor view…";
  const r = await getBankJson(nodeBankUrl(node, `settlements/${encodeURIComponent(key)}/corridor`));
  view.textContent = JSON.stringify(r.body, null, 2);
}

// ---- step 5: credit evidence ---------------------------------------------

async function renderEvidence() {
  const el = $("#evidence-tables");
  if (!el) return;
  const parts = await Promise.all(visibleNodes().map(async (n) => {
    const r = await getBankJson(nodeBankUrl(n.name, "evidence"));
    const rows = r.ok && Array.isArray(r.body) ? r.body : [];
    if (rows.length === 0) {
      return `<h3>${nodeTitle(n.name)}</h3><p class="empty">no received credits</p>`;
    }
    const shown = rows.slice(0, rowLimit(el));
    const body = shown.map((ev) => `
      <tr>
        <td class="id-full">${esc(ev.transaction_request_id)}${
          ev.return_of_transaction_request_id
            ? `<div class="dim">↩ return of ${esc(short(ev.return_of_transaction_request_id, 8))}</div>` : ""}${
          ev.returned_by_transaction_request_id
            ? `<div class="dim">↩ returned by ${esc(short(ev.returned_by_transaction_request_id, 8))}</div>` : ""}${
          ev.forwarded_as_transaction_request_id
            ? `<div class="dim" title="${esc(ev.forwarded_as_transaction_request_id)}">→ carried on as ${esc(short(ev.forwarded_as_transaction_request_id, 8))} on ${esc(ev.forwarded_on_obp_host || "")}</div>` : ""}</td>
        <td>${ev.verified
          ? `<span class="chip chip-verified">verified</span>`
          : `<span class="chip chip-error">MISMATCH</span>`}</td>
        <td class="num">${esc(ev.amount ? `${ev.amount} ${ev.currency || ""}` : "")}</td>
        <td>${esc(ev.originator_name || "")}</td>
        <td>${ev.cbs_status ? chip(ev.cbs_status) : ""} ${esc(ev.cbs_reference || "")}${
          ev.beneficiary_name || ev.beneficiary_account_routing_address
            ? `<div class="dim">→ ${esc(ev.beneficiary_name || "")}</div>
               <div class="id-full">${esc([ev.beneficiary_account_routing_scheme,
                 ev.beneficiary_account_routing_address].filter(Boolean).join(" · "))}</div>`
            : ""}</td>
        <td class="mono" title="${esc(ev.promise_commitment)}">${short(ev.promise_commitment, 16)}</td>
        <td>${esc(ev.received_at ? ev.received_at.slice(0, 19) : "")}</td>
        <td title="${esc(ev.settlement_id || "")}">${ev.settled_at
          ? `<span class="chip chip-verified">settled</span>`
          : `<span class="chip chip-initiated">unsettled</span>`}</td>
      </tr>`).join("");
    return `<h3>${nodeTitle(n.name)}</h3>
      <table>
        <thead><tr><th>transaction request id</th><th>commitment check</th><th>amount</th>
          <th>originator</th><th>CBS delivery</th><th>commitment</th><th>received</th><th>settlement</th></tr></thead>
        <tbody>${body}</tbody>
      </table>
      ${moreLink(rows.length, shown.length, "/credits", "credits")}`;
  }));
  el.innerHTML = parts.join("");
}

// ---- forms ---------------------------------------------------------------

function sendPayload(form) {
  const f = new FormData(form);
  return {
    value: { currency: f.get("currency"), amount: f.get("amount") },
    description: f.get("description"),
    to: {
      name: f.get("beneficiary_name"),
      description: `${f.get("other_account")} at ${f.get("other_bank")}`,
      other_bank_routing_scheme: f.get("other_bank_scheme"),
      other_bank_routing_address: f.get("other_bank"),
      other_account_routing_scheme: f.get("other_account_scheme"),
      other_account_routing_address: f.get("other_account"),
    },
    originator: {
      name: f.get("originator_name"),
      address: f.get("originator_address"),
      account_routing: {
        scheme: f.get("originator_account_scheme"),
        address: f.get("originator_account"),
      },
    },
    charge_policy: "SHARED",
    // Through a hub: the final bank, on another OBP host. Left out for a direct payment.
    ...(String(f.get("final_bank") || "").trim()
      ? { final_bank_routing: { scheme: String(f.get("final_bank_scheme") || "BIC").trim(), address: String(f.get("final_bank")).trim() } }
      : {}),
  };
}

function showResult(el, r) {
  el.hidden = false;
  el.className = "result " + (r.ok ? "result-ok" : "result-err");
  el.textContent = `HTTP ${r.status}\n` + JSON.stringify(r.body, null, 2);
}

async function onSend(e) {
  e.preventDefault();
  const form = e.target;
  const node = form.elements.node.value;
  // Sent by the node's bank on the selected OBP host.
  const url = nodeBankUrl(node, "transaction-requests");
  if (!url) return showResult($("#send-result"), { ok: false, status: 0, body: { message: `${node} is not on OBP host ${CURRENT_HOST}` } });
  const r = await postJson(url, sendPayload(form));
  showResult($("#send-result"), r);
  refresh();
}

async function onSettle(e) {
  e.preventDefault();
  const form = e.target;
  const node = form.elements.node.value;
  // Settled by the node's bank on the selected OBP host.
  const url = nodeBankUrl(node, "settlements");
  if (!url) return showResult($("#settle-result"), { ok: false, status: 0, body: { message: `${node} is not on OBP host ${CURRENT_HOST}` } });
  const r = await postJson(url, {
    other_bank_id: form.elements.other_bank_id.value,
    currency: form.elements.currency.value,
  });
  showResult($("#settle-result"), r);
  refresh();
}

// ---- field history (demo convenience) ------------------------------------

// Each text input in the Send/Settle forms gets a datalist of the last few
// values submitted from this browser (newest first) and is pre-filled with the
// newest one. Last-used wins over configured/HTML defaults; history is keyed
// by field name, so fields shared between forms (e.g. currency) share history.
const HIST_MAX = 8;

function histGet(name) {
  try {
    const v = JSON.parse(localStorage.getItem(`hist:${name}`));
    return Array.isArray(v) ? v : [];
  } catch { return []; }
}

function histAdd(name, value) {
  if (!value) return;
  const vals = [value, ...histGet(name).filter((v) => v !== value)].slice(0, HIST_MAX);
  try { localStorage.setItem(`hist:${name}`, JSON.stringify(vals)); } catch {}
}

function histInputs(form) {
  return [...form.querySelectorAll("input[name]")].filter((i) => !i.type || i.type === "text");
}

function fillHistDatalist(name) {
  const dl = document.getElementById(`hist-${name}`);
  if (dl) dl.innerHTML = histGet(name).map((v) => `<option value="${esc(v)}"></option>`).join("");
}

function initFieldHistory(form) {
  if (!form) return;
  for (const input of histInputs(form)) {
    if (!document.getElementById(`hist-${input.name}`)) {
      const dl = document.createElement("datalist");
      dl.id = `hist-${input.name}`;
      form.append(dl);
    }
    input.setAttribute("list", `hist-${input.name}`);
    input.setAttribute("autocomplete", "off"); // browser autofill fights the datalist
    const hist = histGet(input.name);
    if (hist.length) input.value = hist[0];
    fillHistDatalist(input.name);
  }
  form.addEventListener("submit", () => {
    for (const input of histInputs(form)) {
      histAdd(input.name, input.value.trim());
      fillHistDatalist(input.name);
    }
  });
  // Enter in a text field implicitly submits the form — i.e. fires a payment —
  // and with the datalists that's one keystroke away while picking a value.
  // Only the submit button submits.
  form.addEventListener("keydown", (e) => {
    if (e.key === "Enter" && e.target.matches("input")) e.preventDefault();
  });
}

// ---- boot + polling ------------------------------------------------------

// Fill the Send and Settle node pickers with the selected OBP host's nodes,
// keeping the picked node when it is still there.
function fillNodeSelects() {
  for (const sel of [$("#send-node"), $("#settle-node")]) {
    if (!sel) continue;
    const picked = sel.value;
    sel.innerHTML = visibleNodes().map((n) => `<option value="${esc(n.name)}">${esc(nodeName(n.name))}</option>`).join("");
    if (visibleNodes().some((n) => n.name === picked)) sel.value = picked;
  }
}

// Refill everything in the forms that depends on the selected OBP host.
async function refillForms() {
  fillNodeSelects();
  await fillSendBanks();
  await fillSettleBanks();
  await renderSendNodeAccount();
}

// Apply this app's configured prefill (/api/ui-defaults) to any Send or
// Settle input whose name matches a key. Config wins over the HTML defaults.
async function applyUiDefaults() {
  const forms = [$("#send-form"), $("#settle-form")].filter(Boolean);
  if (forms.length === 0) return;
  const r = await getJson("/api/ui-defaults");
  if (!r.ok || typeof r.body !== "object" || r.body === null) return;
  for (const [name, value] of Object.entries(r.body)) {
    for (const form of forms) {
      const field = form.elements.namedItem(name);
      if (field && field.tagName === "INPUT") field.value = value;
    }
  }
}

// ---- the Network page ------------------------------------------------------

// What the Network page draws, rebuilt on each refresh.
// hosts: [{ host, obpApiUrl, banks: [{ bankId, bic, nodes: [node names on it] }] }]
// payments: [{ host, fromBankId, toBankId, count }]
let NETWORK = { hosts: [], payments: [] };

// Gather, for every OBP host any of this UI's nodes is on: its Open Corridor banks (as the first node on it that answers lists them), which of them are this UI's nodes' banks, and the payments each node's bank has sent there.
async function fetchNetwork() {
  const hosts = [];
  const payments = [];
  for (const hostName of allHosts().filter((name) => name !== UNKNOWN_HOST)) {
    // The UI's nodes on this OBP host.
    const nodesHere = NODES.filter((n) => hostNames(n.name).includes(hostName));
    // The OBP host's banks, from the first of those nodes that can list them.
    let banks = null;
    for (const n of nodesHere) {
      banks = await fetchBanksOn(n.name, hostName);
      if (banks) break;
    }
    // Every Open Corridor bank on the OBP host, with the UI's nodes that serve it.
    const bankRows = (banks || []).filter(isParticipant).map((bank) => ({
      bankId: bank.bank_id,
      bic: bankBic(bank),
      nodes: nodesHere.filter((n) => nodeHostEntry(n.name, hostName)?.bank_id === bank.bank_id).map((n) => n.name),
    }));
    // A node's own bank is drawn even when the bank list could not be fetched.
    for (const n of nodesHere) {
      const ownBankId = nodeHostEntry(n.name, hostName)?.bank_id;
      if (ownBankId && !bankRows.some((row) => row.bankId === ownBankId)) {
        bankRows.push({ bankId: ownBankId, bic: "", nodes: [n.name] });
      }
    }
    hosts.push({ host: hostName, obpApiUrl: hostUrl(hostName), banks: bankRows, listed: banks !== null });
    // The payments each node's bank sent on this OBP host, counted per receiving bank.
    for (const n of nodesHere) {
      const entry = nodeHostEntry(n.name, hostName);
      const url = nodeUrl(n.name, `hosts/${encodeURIComponent(entry.host)}/banks/${encodeURIComponent(entry.bank_id)}/transaction-requests`);
      const r = await getJson(url);
      const rows = r.ok && Array.isArray(r.body) ? r.body : [];
      const counts = {};
      for (const tr of rows) {
        if (tr.other_bank_id) counts[tr.other_bank_id] = (counts[tr.other_bank_id] || 0) + 1;
      }
      for (const [toBankId, count] of Object.entries(counts)) {
        payments.push({ host: hostName, fromBankId: entry.bank_id, toBankId, count });
      }
    }
  }
  NETWORK = { hosts, payments };
}

// A BIC that is on more than one of the drawn OBP hosts belongs to a hub.
function hubBics() {
  const seen = {};
  for (const host of NETWORK.hosts) {
    for (const bic of new Set(host.banks.map((bank) => bank.bic).filter(Boolean))) {
      seen[bic] = (seen[bic] || 0) + 1;
    }
  }
  return new Set(Object.keys(seen).filter((bic) => seen[bic] > 1));
}

// Shorten a label for the drawing.
function clip(text, max) {
  return text.length > max ? `${text.slice(0, max - 1)}…` : text;
}

// Draw the OBP hosts side by side, each bank as a circle, hubs joined across OBP hosts, and payment lines between banks.
function renderNetworkView() {
  const el = $("#network-view");
  if (!el) return;
  if (NETWORK.hosts.length === 0) {
    el.innerHTML = `<p class="empty">no OBP host known yet: no node has answered /status</p>`;
    return;
  }
  const hubs = hubBics();
  // Sizes of the drawing.
  const boxWidth = 380, boxHeight = 400, gap = 140, top = 40, bankRadius = 24;
  const width = NETWORK.hosts.length * boxWidth + (NETWORK.hosts.length - 1) * gap + 20;
  const height = boxHeight + top + 20;
  // Where each bank is drawn: "host|bankId" -> {x, y}.
  const position = {};
  const boxes = NETWORK.hosts.map((host, hostIndex) => {
    const boxX = 10 + hostIndex * (boxWidth + gap);
    const centreX = boxX + boxWidth / 2, centreY = top + boxHeight / 2 + 10;
    // Hubs first, so they sit on the side facing the next OBP host (or the previous one for the last box).
    const banks = [...host.banks].sort((a, b) => (hubs.has(b.bic) ? 1 : 0) - (hubs.has(a.bic) ? 1 : 0));
    const facingAngle = hostIndex === NETWORK.hosts.length - 1 && hostIndex > 0 ? Math.PI : 0;
    const ringRadius = banks.length > 1 ? 120 : 0;
    banks.forEach((bank, bankIndex) => {
      const angle = facingAngle + (2 * Math.PI * bankIndex) / banks.length;
      position[`${host.host}|${bank.bankId}`] = {
        x: centreX + ringRadius * Math.cos(angle),
        y: centreY + ringRadius * Math.sin(angle),
      };
    });
    const note = host.listed ? "" : `<text x="${centreX}" y="${top + boxHeight - 12}" class="net-note" text-anchor="middle">could not list this OBP host's banks</text>`;
    return `<g>
      <rect x="${boxX}" y="${top}" width="${boxWidth}" height="${boxHeight}" rx="14" class="net-host"/>
      <text x="${boxX + 14}" y="${top + 24}" class="net-host-name">OBP host ${esc(host.host)}</text>
      ${note}
    </g>`;
  });
  // Payment lines inside each OBP host, labelled with the number of payments near the sending end, so lines both ways between two banks keep their labels apart.
  const paymentLines = NETWORK.payments.map((payment) => {
    const from = position[`${payment.host}|${payment.fromBankId}`];
    const to = position[`${payment.host}|${payment.toBankId}`];
    if (!from || !to) return "";
    const label = payment.count === 1 ? "1 payment" : `${payment.count} payments`;
    return `<g>
      <line x1="${from.x}" y1="${from.y}" x2="${to.x}" y2="${to.y}" class="net-payment" marker-end="url(#net-arrow)"/>
      <text x="${from.x + (to.x - from.x) / 3}" y="${from.y + (to.y - from.y) / 3 - 6}" class="net-edge-label" text-anchor="middle">${label}</text>
    </g>`;
  }).join("");
  // Dashed lines joining a hub's banks on neighbouring OBP hosts.
  const hubLines = [];
  for (let hostIndex = 0; hostIndex + 1 < NETWORK.hosts.length; hostIndex++) {
    const left = NETWORK.hosts[hostIndex], right = NETWORK.hosts[hostIndex + 1];
    for (const leftBank of left.banks.filter((bank) => hubs.has(bank.bic))) {
      const rightBank = right.banks.find((bank) => bank.bic === leftBank.bic);
      if (!rightBank) continue;
      const from = position[`${left.host}|${leftBank.bankId}`];
      const to = position[`${right.host}|${rightBank.bankId}`];
      hubLines.push(`<g>
        <line x1="${from.x}" y1="${from.y}" x2="${to.x}" y2="${to.y}" class="net-hub-link"/>
        <text x="${(from.x + to.x) / 2}" y="${(from.y + to.y) / 2 - 8}" class="net-edge-label" text-anchor="middle">hub · BIC ${esc(leftBank.bic)}</text>
      </g>`);
    }
  }
  // The banks, drawn last so they sit on top of the lines.
  const bankMarks = NETWORK.hosts.flatMap((host) => host.banks.map((bank) => {
    const point = position[`${host.host}|${bank.bankId}`];
    // A bank can be both: a hub node's bank is served by this UI and is a hub.
    const kind = ["net-bank", bank.nodes.length ? "own" : "", hubs.has(bank.bic) ? "hub" : ""].join(" ").trim();
    const nodeLine = bank.nodes.length ? `<text x="${point.x}" y="${point.y + 4}" class="net-node-name" text-anchor="middle">${esc(clip(bank.nodes.map(nodeName).join(", "), 10))}</text>` : "";
    const title = `${bank.bankId}${bank.bic ? ` · BIC ${bank.bic}` : ""} on ${host.host}${bank.nodes.length ? ` · node ${bank.nodes.map(nodeName).join(", ")}` : ""}${hubs.has(bank.bic) ? " · hub" : ""}`;
    return `<g>
      <title>${esc(title)}</title>
      <circle cx="${point.x}" cy="${point.y}" r="${bankRadius}" class="${kind}"/>
      ${nodeLine}
      <text x="${point.x}" y="${point.y + bankRadius + 14}" class="net-bank-id" text-anchor="middle">${esc(clip(bank.bankId, 22))}</text>
      <text x="${point.x}" y="${point.y + bankRadius + 28}" class="net-bic" text-anchor="middle">${esc(bank.bic ? `BIC ${bank.bic}` : "no BIC")}</text>
    </g>`;
  })).join("");
  el.innerHTML = `<svg class="net-svg" viewBox="0 0 ${width} ${height}" width="${width}" height="${height}" role="img" aria-label="Open Corridor banks per OBP host">
    <defs><marker id="net-arrow" viewBox="0 0 10 10" refX="27" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M0,0 L10,5 L0,10 z" class="net-arrow"/></marker></defs>
    ${boxes.join("")}${hubLines.join("")}${paymentLines}${bankMarks}
  </svg>
  <p class="hint net-legend"><span class="net-key own"></span> a bank served by one of this UI's nodes · <span class="net-key hub"></span> a hub (the same BIC on two OBP hosts) · <span class="net-key"></span> another Open Corridor bank</p>`;
}

// Under the drawing: per OBP host, the banks as text, for reading and copying.
function renderNetworkHosts() {
  const el = $("#network-hosts");
  if (!el) return;
  const hubs = hubBics();
  el.innerHTML = NETWORK.hosts.map((host) => {
    const rows = host.banks.map((bank) => {
      const sent = NETWORK.payments.filter((p) => p.host === host.host && p.fromBankId === bank.bankId)
        .map((p) => `${p.count} to ${p.toBankId}`).join(", ");
      return `<tr>
        <td>${esc(bank.bankId)}</td>
        <td>${esc(bank.bic)}</td>
        <td>${esc(bank.nodes.map(nodeName).join(", "))}</td>
        <td>${hubs.has(bank.bic) ? "hub" : ""}</td>
        <td>${esc(sent)}</td>
      </tr>`;
    }).join("");
    return `<h3>OBP host ${esc(host.host)} <span class="dim">· OBP-API ${esc(host.obpApiUrl)}</span></h3>
      <table>
        <thead><tr><th>bank id</th><th>BIC</th><th>this UI's node</th><th>role</th><th>payments sent</th></tr></thead>
        <tbody>${rows || `<tr><td colspan="5" class="empty">no Open Corridor banks listed</td></tr>`}</tbody>
      </table>`;
  }).join("");
}

// One app.js serves every page: each renderer no-ops when its container is
// absent, so refresh() only fetches what the current page shows.
async function refresh() {
  // Node identity and OBP hosts, used by the headings below (cached per node).
  await Promise.all(NODES.map((n) => fetchStatus(n.name)));
  // A node that has just answered can bring a new OBP host: redraw the tabs, and the forms if the selection moved.
  if (renderHostTabs()) await refillForms();
  // Each node's banks, so the tables can show a BIC next to each bank_id (cached per node).
  await Promise.all(NODES.map((n) => fetchBanks(n.name)));
  if ($("#tr-tables") || $("#position-view")) {
    await fetchTrs();
    renderTrTables();
    renderPosition();
  }
  if ($("#settlement-tables") || $("#latest-settlement")) {
    await fetchSettlements();
    renderLatestSettlement();
  }
  if ($("#network-view")) {
    await fetchNetwork();
    renderNetworkView();
    renderNetworkHosts();
  }
  await Promise.all([renderHealth(), renderSettlements(), renderEvidence()]);
}

// Restore each section's collapsed/open state from localStorage and persist toggles.
function initSectionFolds() {
  for (const d of document.querySelectorAll("main section > details")) {
    const key = `fold:${d.id}`;
    const saved = localStorage.getItem(key);
    if (saved !== null) d.open = saved === "open";
    d.addEventListener("toggle", () => localStorage.setItem(key, d.open ? "open" : "closed"));
  }
}

async function boot() {
  initSectionFolds();
  const r = await getJson("/api/nodes");
  NODES = r.ok && Array.isArray(r.body) ? r.body : [];
  // Each node's OBP host, for the tabs, before anything is drawn.
  await Promise.all(NODES.map((n) => fetchStatus(n.name)));
  renderHostTabs();
  fillNodeSelects();
  await applyUiDefaults();
  initFieldHistory($("#send-form"));
  initFieldHistory($("#settle-form"));
  $("#send-form")?.addEventListener("submit", onSend);
  $("#settle-form")?.addEventListener("submit", onSettle);
  $("#send-node")?.addEventListener("change", () => {
    fillSendBanks();
    renderSendNodeAccount();
  });
  $("#send-other-bank")?.addEventListener("change", (e) => applySendBank(e.target.value));
  $("#send-form")?.elements.namedItem("currency")
    ?.addEventListener("input", renderSendNodeAccount);
  $("#settle-node")?.addEventListener("change", fillSettleBanks);
  await fillSendBanks();
  await fillSettleBanks();
  document.addEventListener("click", (e) => {
    const btn = e.target.closest(".corridor-btn");
    if (btn) showCorridor(btn.dataset.node, btn.dataset.key);
    const tab = e.target.closest(".host-tab");
    if (tab && tab.dataset.host !== CURRENT_HOST) selectHost(tab.dataset.host);
  });
  await refresh();
  setInterval(() => { if (!document.hidden) refresh(); }, POLL_MS);
}

boot();
