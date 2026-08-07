// OBP Bank Node App — vanilla JS, no build step.
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

// ---- header: node health -------------------------------------------------

async function renderHealth() {
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "health"));
    const up = r.ok && r.body && r.body.status === "healthy";
    const chain = up ? ` · ${esc(r.body.blockchain)}` : "";
    return `<span class="node-chip ${up ? "up" : "down"}">${esc(n.name)}${chain}</span>`;
  }));
  $("#node-health").innerHTML = parts.join("");
}

// ---- step 2: promises (outbox tables) ------------------------------------

let TRS = {}; // node name -> array of transaction request status rows

async function fetchTrs() {
  for (const n of NODES) {
    const r = await getJson(nodeUrl(n.name, "transaction-requests"));
    TRS[n.name] = r.ok && Array.isArray(r.body) ? r.body : [];
  }
}

function renderTrTables() {
  const html = NODES.map((n) => {
    const rows = TRS[n.name] || [];
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no outbound transaction requests</p>`;
    }
    const body = rows.map((tr) => `
      <tr>
        <td title="${esc(tr.transaction_request_id)}">${short(tr.transaction_request_id)}</td>
        <td>${chip(tr.status)}</td>
        <td class="num">${esc(tr.value ? `${tr.value.amount} ${tr.value.currency}` : "")}</td>
        <td>${esc(tr.other_bank_id || "")}</td>
        <td>${tr.promise_id ? txLink(tr.promise_id) : ""}</td>
        <td title="${esc(tr.settlement_id || "")}">${short(tr.settlement_id)}</td>
        <td>${esc(tr.settled_at ? tr.settled_at.slice(0, 19) : "")}</td>
      </tr>`).join("");
    return `<h3>${esc(n.name)}</h3>
      <table>
        <thead><tr><th>id</th><th>status</th><th>value</th><th>to bank</th>
          <th>promise tx</th><th>settlement</th><th>settled at</th></tr></thead>
        <tbody>${body}</tbody>
      </table>`;
  }).join("");
  $("#tr-tables").innerHTML = html;
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
  $("#position-view").innerHTML = html;
}

// ---- step 4: settlements -------------------------------------------------

async function renderSettlements() {
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "settlements"));
    const rows = r.ok && Array.isArray(r.body) ? r.body : [];
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no settlements</p>`;
    }
    const body = rows.map((s) => `
      <tr>
        <td title="${esc(s.idempotency_key)}">${short(s.settlement_id || s.idempotency_key)}</td>
        <td>${chip(s.status)}</td>
        <td class="num">${esc(s.net_amount_minor)} <span class="dim">minor ${esc(s.currency)}</span></td>
        <td class="num">${esc(s.asset_amount ? `${s.asset_amount} ${s.asset || ""}` : "")}</td>
        <td class="num">${s.depth}/${s.finality_depth}</td>
        <td>${txLink(s.tx_id)}</td>
        <td>${esc(s.error_reason || "")}</td>
        <td><button class="small corridor-btn" data-node="${esc(n.name)}"
              data-key="${esc(s.settlement_id || s.idempotency_key)}">corridor</button></td>
      </tr>`).join("");
    return `<h3>${esc(n.name)}</h3>
      <table>
        <thead><tr><th>id</th><th>status</th><th>net</th><th>rail amount</th>
          <th>depth</th><th>tx</th><th>error</th><th></th></tr></thead>
        <tbody>${body}</tbody>
      </table>`;
  }));
  $("#settlement-tables").innerHTML = parts.join("");
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
  const parts = await Promise.all(NODES.map(async (n) => {
    const r = await getJson(nodeUrl(n.name, "evidence"));
    const rows = r.ok && Array.isArray(r.body) ? r.body : [];
    if (rows.length === 0) {
      return `<h3>${esc(n.name)}</h3><p class="empty">no received credits</p>`;
    }
    const body = rows.map((ev) => `
      <tr>
        <td title="${esc(ev.transaction_request_id)}">${short(ev.transaction_request_id)}</td>
        <td>${ev.verified
          ? `<span class="chip chip-verified">verified</span>`
          : `<span class="chip chip-error">MISMATCH</span>`}</td>
        <td class="num">${esc(ev.amount ? `${ev.amount} ${ev.currency || ""}` : "")}</td>
        <td>${esc(ev.originator_name || "")}</td>
        <td>${ev.cbs_status ? chip(ev.cbs_status) : ""} ${esc(ev.cbs_reference || "")}</td>
        <td class="mono" title="${esc(ev.promise_commitment)}">${short(ev.promise_commitment, 16)}</td>
        <td>${esc(ev.received_at ? ev.received_at.slice(0, 19) : "")}</td>
      </tr>`).join("");
    return `<h3>${esc(n.name)}</h3>
      <table>
        <thead><tr><th>tr id</th><th>commitment check</th><th>amount</th>
          <th>originator</th><th>CBS delivery</th><th>commitment</th><th>received</th></tr></thead>
        <tbody>${body}</tbody>
      </table>`;
  }));
  $("#evidence-tables").innerHTML = parts.join("");
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

// ---- boot + polling ------------------------------------------------------

function fillNodeSelects() {
  for (const sel of [$("#send-node"), $("#settle-node")]) {
    sel.innerHTML = NODES.map((n) => `<option>${esc(n.name)}</option>`).join("");
  }
}

// Apply the instance's configured prefill (/api/ui-defaults) to any Send or
// Settle input whose name matches a key. Config wins over the HTML defaults.
async function applyUiDefaults() {
  const r = await getJson("/api/ui-defaults");
  if (!r.ok || typeof r.body !== "object" || r.body === null) return;
  for (const [name, value] of Object.entries(r.body)) {
    for (const form of [$("#send-form"), $("#settle-form")]) {
      const field = form.elements.namedItem(name);
      if (field && field.tagName === "INPUT") field.value = value;
    }
  }
}

async function refresh() {
  await fetchTrs();
  renderTrTables();
  renderPosition();
  await Promise.all([renderHealth(), renderSettlements(), renderEvidence()]);
}

async function boot() {
  const r = await getJson("/api/nodes");
  NODES = r.ok && Array.isArray(r.body) ? r.body : [];
  fillNodeSelects();
  await applyUiDefaults();
  $("#send-form").addEventListener("submit", onSend);
  $("#settle-form").addEventListener("submit", onSettle);
  document.addEventListener("click", (e) => {
    const btn = e.target.closest(".corridor-btn");
    if (btn) showCorridor(btn.dataset.node, btn.dataset.key);
  });
  await refresh();
  setInterval(() => { if (!document.hidden) refresh(); }, POLL_MS);
}

boot();
