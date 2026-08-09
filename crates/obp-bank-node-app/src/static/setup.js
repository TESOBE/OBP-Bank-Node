// Setup page — vanilla JS, no build step. Render the check list; every
// mutation is an explicit Apply that names an item id, nothing more.

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

function esc(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({
    "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
  })[c]);
}

const STATUS_CHIP = {
  ok: "chip-verified",
  missing: "chip-submitted",
  differs: "chip-submitted",
  unverified: "chip-initiated",
  error: "chip-error",
};

function chip(status) {
  return `<span class="chip ${STATUS_CHIP[status] || ""}">${esc(status)}</span>`;
}

const SECTIONS = [
  ["me", "Your entitlements"],
  ["schemes", "Routing schemes"],
  ["banks", "Banks, accounts, FX, brokers"],
  ["grants", "Role grants (existing users)"],
];

// Items an Apply makes sense for; "error" items name a cause the apply
// cannot fix (unreachable API, user that does not exist, …).
const APPLYABLE = new Set(["missing", "differs", "unverified"]);

let ITEMS = [];
let LAST_STATUS = null; // raw /api/setup/status body, feeds the JSON block

// Next to every Apply: request the role the apply needs, for admins who
// cannot self-grant. Someone with granting rights approves it in OBP.
function roleButton(i) {
  const rr = i.required_role || {};
  if (!rr.role) return "";
  const label = rr.bank_id ? `${rr.role} (${rr.bank_id})` : rr.role;
  return `<button class="small request-btn" data-bank="${esc(rr.bank_id)}"
            data-role="${esc(rr.role)}" title="Request ${esc(label)} for yourself">Request role</button>`;
}

function renderItems() {
  const html = SECTIONS.map(([key, title]) => {
    const items = ITEMS.filter((i) => i.section === key);
    if (items.length === 0) return "";
    const rows = items.map((i) => `
      <tr>
        <td>${esc(i.title)}</td>
        <td>${chip(i.status)}</td>
        <td class="dim">${esc(i.detail)}</td>
        <td class="actions">${APPLYABLE.has(i.status)
          ? `<button class="small apply-btn" data-id="${esc(i.id)}">Apply</button> ${roleButton(i)}`
          : ""}</td>
      </tr>`).join("");
    return `<h3>${esc(title)}</h3>
      <table>
        <thead><tr><th>item</th><th>status</th><th>detail</th><th></th></tr></thead>
        <tbody>${rows}</tbody>
      </table>`;
  }).join("");
  $("#setup-items").innerHTML = html || `<p class="empty">nothing configured to check</p>`;
}

// A copy-pasteable machine-readable snapshot for handing to an agent working
// on the Bank Node or this app: the full status body plus the app instance's
// node topology and health, read through the existing proxy.
async function renderStatusJson() {
  if (!LAST_STATUS) return;
  const nodesResp = await getJson("/api/nodes");
  const nodes = nodesResp.ok && Array.isArray(nodesResp.body) ? nodesResp.body : [];
  const nodeHealth = await Promise.all(nodes.map(async (n) => {
    const h = await getJson(`/api/nodes/${encodeURIComponent(n.name)}/health`);
    return { name: n.name, reachable: h.ok, health: h.body };
  }));
  const snapshot = {
    snapshot_type: "obp-bank-node-app-setup-status",
    app_url: location.origin,
    nodes: nodeHealth,
    ...LAST_STATUS,
  };
  $("#status-json").textContent = JSON.stringify(snapshot, null, 2);
}

async function refresh() {
  const r = await getJson("/api/setup/status");
  if (r.status === 401) return showLoggedOut();
  if (!r.ok) {
    $("#setup-items").innerHTML =
      `<p class="empty">status failed: ${esc(r.body && r.body.message)}</p>`;
    return;
  }
  LAST_STATUS = r.body;
  ITEMS = Array.isArray(r.body.items) ? r.body.items : [];
  // One app instance = one bank: test accounts go to the own bank.
  if (r.body.bank) {
    const bankField = $("#test-account-form").elements.namedItem("bank_id");
    bankField.value = r.body.bank;
    bankField.readOnly = true;
  }
  const api = r.body.api || {};
  $("#api-info").textContent = api.status === "ok"
    ? `logged in as ${r.body.me.username}`
    : `OBP-API unreachable: ${api.detail || ""}`;
  renderItems();
  await renderStatusJson();
}

function showResult(el, r) {
  el.hidden = false;
  el.className = "result " + (r.ok && (!r.body || r.body.ok !== false) ? "result-ok" : "result-err");
  el.textContent = `HTTP ${r.status}\n` + JSON.stringify(r.body, null, 2);
}

async function applyOne(id) {
  const r = await postJson("/api/setup/apply", { id });
  showResult($("#apply-result"), r);
  await refresh();
}

async function applyAllMissing() {
  const out = [];
  // Sequential on purpose: later items depend on earlier ones (a bank must
  // exist before its accounts; a role before the create it authorizes).
  for (const i of ITEMS.filter((i) => APPLYABLE.has(i.status))) {
    const r = await postJson("/api/setup/apply", { id: i.id });
    out.push(`${i.id}: HTTP ${r.status} ${r.body && r.body.ok ? "ok" : "FAILED"}`);
  }
  const el = $("#apply-result");
  el.hidden = false;
  el.className = "result";
  el.textContent = out.join("\n") || "nothing to apply";
  await refresh();
}

function showLoggedOut() {
  $("#login-section").hidden = false;
  $("#status-section").hidden = true;
  $("#status-json-section").hidden = true;
  $("#test-account-section").hidden = true;
  $("#session-box").innerHTML = `<span class="node-chip down">logged out</span>`;
}

function showLoggedIn(me) {
  $("#login-section").hidden = true;
  $("#status-section").hidden = false;
  $("#status-json-section").hidden = false;
  $("#test-account-section").hidden = false;
  $("#session-box").innerHTML =
    `<span class="node-chip up">${esc(me.username)}</span>
     <button id="logout-btn" class="small">Log out</button>`;
}

async function onTestAccount(e) {
  e.preventDefault();
  const f = new FormData(e.target);
  const r = await postJson("/api/setup/test-account", {
    bank_id: f.get("bank_id"),
    account_id: f.get("account_id"),
    label: f.get("label"),
    currency: f.get("currency"),
    owner_username: f.get("owner_username") || null,
  });
  showResult($("#test-account-result"), r);
}

async function boot() {
  $("#login-btn").addEventListener("click", () => { location.href = "/setup/login"; });
  $("#refresh-btn").addEventListener("click", refresh);
  $("#apply-all-btn").addEventListener("click", applyAllMissing);
  $("#test-account-form").addEventListener("submit", onTestAccount);
  $("#copy-json-btn").addEventListener("click", async () => {
    const text = $("#status-json").textContent;
    try {
      await navigator.clipboard.writeText(text);
      $("#copy-json-btn").textContent = "Copied";
    } catch {
      // Clipboard API needs a secure context; fall back to selecting the
      // block so a manual Ctrl-C works.
      const range = document.createRange();
      range.selectNodeContents($("#status-json"));
      const sel = getSelection();
      sel.removeAllRanges();
      sel.addRange(range);
      $("#copy-json-btn").textContent = "Selected — press Ctrl-C";
    }
    setTimeout(() => { $("#copy-json-btn").textContent = "Copy"; }, 2000);
  });
  document.addEventListener("click", async (e) => {
    if (e.target.closest("#logout-btn")) {
      await postJson("/setup/logout", {});
      showLoggedOut();
      return;
    }
    const applyBtn = e.target.closest(".apply-btn");
    if (applyBtn) { applyOne(applyBtn.dataset.id); return; }
    const requestBtn = e.target.closest(".request-btn");
    if (requestBtn) {
      const r = await postJson("/api/setup/request-entitlement", {
        bank_id: requestBtn.dataset.bank,
        role: requestBtn.dataset.role,
      });
      showResult($("#apply-result"), r);
    }
  });

  const me = await getJson("/api/setup/me");
  if (me.status === 401) {
    showLoggedOut();
    return;
  }
  if (!me.ok) {
    // Not a login problem — the app or OBP-API failed. Say so rather than
    // rendering it as "logged out".
    showLoggedOut();
    const el = $("#login-error");
    el.hidden = false;
    el.textContent = me.body && me.body.message
      ? `${me.body.error_code || "error"}: ${me.body.message}`
      : `HTTP ${me.status}`;
    return;
  }
  showLoggedIn(me.body);
  await refresh();
}

boot();
