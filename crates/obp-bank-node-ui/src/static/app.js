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

// Per-node identity from /health (bank_id, account_id) — drives the
// beneficiary-default swap when the sending node changes.
const NODE_INFO = {};

async function renderHealth() {
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "health"));
    const up = r.ok && r.body && r.body.status === "healthy";
    if (up) NODE_INFO[n.name] = { bank_id: r.body.bank_id, account_id: r.body.account_id };
    const chain = up ? ` · ${esc(r.body.blockchain)}` : "";
    // "disabled" is a deliberate config; anything else non-connected means the
    // node cannot receive settlement instructions or credit notifications.
    const ifc = up && r.body.interface_c && !["connected", "disabled"].includes(r.body.interface_c);
    if (ifc) {
      return `<span class="node-chip warn" title="${esc(r.body.interface_c_detail || r.body.interface_c)}">
        ${esc(n.name)} · up${chain} · AMQP ${esc(r.body.interface_c)}</span>`;
    }
    return `<span class="node-chip ${up ? "up" : "down"}">${esc(n.name)} · ${up ? "up" : "down"}${chain}</span>`;
  }));
  $("#node-health").innerHTML = parts.join("");
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

// Read-only line under the Sending node select: the corridor account the
// promise TR for the entered currency will be booked on — or a warning when
// the node has no account for that currency (the node would refuse with
// OBP-BANK-NODE-ROUTING-001).
async function renderSendNodeAccount() {
  const el = $("#send-node-account");
  if (!el) return;
  const node = $("#send-node")?.value;
  if (!node) { el.textContent = ""; return; }
  const status = await fetchStatus(node);
  const currency = ($("#send-form")?.elements.namedItem("currency")?.value || "")
    .trim().toUpperCase();
  const map = status?.corridor_accounts || {};
  if (Object.keys(map).length && currency) {
    el.textContent = map[currency]
      ? `corridor account: ${status.bank_id} / ${map[currency]}`
      : `no corridor account for ${currency} — the node will refuse this send`;
    return;
  }
  const info = status || NODE_INFO[node];
  el.textContent = info ? `corridor account: ${info.bank_id} / ${info.account_id}` : "";
}

// A corridor is inter-bank: when the sender changes, point the beneficiary
// fields at the first OTHER node's bank/account so the defaults never target
// the sending bank itself.
function swapBeneficiaryDefaults(senderName) {
  const sender = NODE_INFO[senderName];
  const other = NODES.map((n) => n.name)
    .filter((name) => name !== senderName && NODE_INFO[name])
    .map((name) => NODE_INFO[name])
    .find((info) => !sender || info.bank_id !== sender.bank_id);
  if (!other) return;
  const send = $("#send-form").elements;
  send.namedItem("other_bank").value = other.bank_id;
  send.namedItem("other_account").value = other.account_id;
  send.namedItem("beneficiary_name").value = `Beneficiary at ${other.bank_id}`;
  $("#settle-form").elements.namedItem("other_bank_id").value = other.bank_id;
}

// ---- step 2: promises (outbox tables) ------------------------------------

let TRS = {}; // node name -> array of transaction request status rows

async function fetchTrs() {
  for (const n of NODES) {
    const r = await getJson(nodeUrl(n.name, "transaction-requests"));
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
  const html = NODES.map((n) => {
    const rows = TRS[n.name] || [];
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no outbound transaction requests</p>`;
    }
    const shown = rows.slice(0, rowLimit(el));
    const body = shown.map((tr) => `
      <tr>
        <td class="id-full">${esc(tr.transaction_request_id)}</td>
        <td>${chip(tr.status)}</td>
        <td class="num">${esc(tr.value ? `${tr.value.amount} ${tr.value.currency}` : "")}</td>
        <td>${esc(tr.other_bank_id || "")}</td>
        <td>${tr.promise_id ? txLink(tr.promise_id) : ""}</td>
        <td class="id-full">${esc(tr.settlement_id || "")}</td>
        <td>${esc(tr.settled_at ? tr.settled_at.slice(0, 19) : "")}</td>
        <td class="details">${esc(trDetails(tr))}</td>
      </tr>`).join("");
    return `<h3>${esc(n.name)}</h3>
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
  const html = NODES.map((n) => {
    const corridors = outboundExposure(TRS[n.name] || []);
    if (corridors.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no unsettled outbound corridor legs</p>`;
    }
    const rows = corridors.map((c) => `<tr>
      <td>${esc(c.other)}</td>
      <td>${esc(c.currency)}</td>
      <td class="num net">${c.total.toFixed(2)}</td>
      <td class="num">${c.count}</td>
    </tr>`).join("");
    return `<h3>${esc(n.name)}</h3>
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
        <td>${esc(s.debtor_bank_id || "")}</td>
        <td class="num">${esc(s.net_amount || "")} ${esc(s.currency || "")}</td>
        <td class="num">${esc(fmtRail(s.received_amount || s.asset_amount, s.asset))}</td>
        <td>${txLink(s.tx_id)}</td>
        <td class="details">${esc(s.detail || "")}</td>
      </tr>`).join("");
  return `<h4>${esc(n.name)} — incoming (as creditor)</h4>
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
  const entries = await Promise.all(NODES.map(async (n) => {
    const [r, inc] = await Promise.all([
      getJson(nodeUrl(n.name, "settlements")),
      getJson(nodeUrl(n.name, "incoming-settlements")),
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
  const from = o ? latest.payer : (i.debtor_bank_id || "?");
  const to = i ? latest.payee : "";
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
  const parts = NODES.map((n) => {
    const data = SETTLEMENTS[n.name] || { out: [], in: [] };
    const rows = data.out;
    const incoming = incomingTable(n, data.in.slice(0, rowLimit(el)));
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no outgoing settlements</p>${incoming}`;
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
    return `<h3>${esc(n.name)}</h3>
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
  const r = await getJson(nodeUrl(node, `settlements/${encodeURIComponent(key)}/corridor`));
  view.textContent = JSON.stringify(r.body, null, 2);
}

// ---- step 5: credit evidence ---------------------------------------------

async function renderEvidence() {
  const el = $("#evidence-tables");
  if (!el) return;
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "evidence"));
    const rows = r.ok && Array.isArray(r.body) ? r.body : [];
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no received credits</p>`;
    }
    const shown = rows.slice(0, rowLimit(el));
    const body = shown.map((ev) => `
      <tr>
        <td class="id-full">${esc(ev.transaction_request_id)}${
          ev.return_of_transaction_request_id
            ? `<div class="dim">↩ return of ${esc(short(ev.return_of_transaction_request_id, 8))}</div>` : ""}${
          ev.returned_by_transaction_request_id
            ? `<div class="dim">↩ returned by ${esc(short(ev.returned_by_transaction_request_id, 8))}</div>` : ""}</td>
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
    return `<h3>${esc(n.name)}</h3>
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
  const r = await postJson(nodeUrl(node, "transaction-requests"), sendPayload(form));
  showResult($("#send-result"), r);
  refresh();
}

async function onSettle(e) {
  e.preventDefault();
  const form = e.target;
  const node = form.elements.node.value;
  const r = await postJson(nodeUrl(node, "settlements"), {
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

function fillNodeSelects() {
  for (const sel of [$("#send-node"), $("#settle-node")]) {
    if (sel) sel.innerHTML = NODES.map((n) => `<option>${esc(n.name)}</option>`).join("");
  }
}

// Apply the instance's configured prefill (/api/ui-defaults) to any Send or
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

// One app.js serves every page: each renderer no-ops when its container is
// absent, so refresh() only fetches what the current page shows.
async function refresh() {
  if ($("#tr-tables") || $("#position-view")) {
    await fetchTrs();
    renderTrTables();
    renderPosition();
  }
  if ($("#settlement-tables") || $("#latest-settlement")) {
    await fetchSettlements();
    renderLatestSettlement();
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
  fillNodeSelects();
  await applyUiDefaults();
  initFieldHistory($("#send-form"));
  initFieldHistory($("#settle-form"));
  $("#send-form")?.addEventListener("submit", onSend);
  $("#settle-form")?.addEventListener("submit", onSettle);
  $("#send-node")?.addEventListener("change", (e) => {
    swapBeneficiaryDefaults(e.target.value);
    renderSendNodeAccount();
  });
  $("#send-form")?.elements.namedItem("currency")
    ?.addEventListener("input", renderSendNodeAccount);
  $("#settle-node")?.addEventListener("change", (e) => swapBeneficiaryDefaults(e.target.value));
  document.addEventListener("click", (e) => {
    const btn = e.target.closest(".corridor-btn");
    if (btn) showCorridor(btn.dataset.node, btn.dataset.key);
  });
  await refresh();
  setInterval(() => { if (!document.hidden) refresh(); }, POLL_MS);
}

boot();
