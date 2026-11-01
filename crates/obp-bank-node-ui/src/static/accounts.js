// Accounts page — vanilla JS, no build step. Renders the configured bank's
// account directory (relayed verbatim from OBP-API by the app backend).
// Session is the same OIDC login the setup page uses.

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

function routingLines(routings) {
  if (!Array.isArray(routings) || routings.length === 0) return "";
  return routings.map((r) =>
    `<div><span class="dim">${esc(r.scheme)}</span> ${esc(r.address)}</div>`
  ).join("");
}

function renderAccounts(body) {
  const accounts = Array.isArray(body.accounts) ? body.accounts : [];
  const bank = accounts[0] && accounts[0].bank_id;
  $("#directory-info").textContent = accounts.length
    ? `${accounts.length} accounts${bank ? ` at ${bank}` : ""}`
    : "no accounts";
  if (accounts.length === 0) {
    $("#accounts-table").innerHTML = `<p class="empty">the directory is empty</p>`;
    return;
  }
  const rows = accounts.map((a) => `
    <tr>
      <td class="mono">${esc(a.account_id)}</td>
      <td>${esc(a.label)}</td>
      <td>${esc(a.account_type)}</td>
      <td class="mono">${esc(a.account_number)}</td>
      <td class="mono">${routingLines(a.account_routings)}</td>
      <td class="dim">${esc((a.view_ids || []).join(", "))}</td>
    </tr>`).join("");
  $("#accounts-table").innerHTML = `
    <table>
      <thead><tr>
        <th>account id</th><th>label</th><th>type</th><th>number</th>
        <th>routings</th><th>views</th>
      </tr></thead>
      <tbody>${rows}</tbody>
    </table>`;
}

async function refresh() {
  const r = await getJson("/api/setup/account-directory");
  if (r.status === 401) return showLoggedOut();
  const err = $("#directory-error");
  if (!r.ok) {
    err.hidden = false;
    err.textContent = `HTTP ${r.status}\n` + JSON.stringify(r.body, null, 2);
    $("#accounts-table").innerHTML = "";
    $("#directory-info").textContent = "";
    return;
  }
  err.hidden = true;
  renderAccounts(r.body);
}

function showLoggedOut() {
  $("#login-section").hidden = false;
  $("#accounts-section").hidden = true;
  $("#session-box").innerHTML = `<span class="node-chip down">logged out</span>`;
}

function showLoggedIn(me) {
  $("#login-section").hidden = true;
  $("#accounts-section").hidden = false;
  $("#session-box").innerHTML =
    `<span class="node-chip up">${esc(me.username)}</span>
     <button id="logout-btn" class="small">Log out</button>`;
}

async function boot() {
  $("#login-btn").addEventListener("click", () => {
    location.href = "/setup/login?next=/accounts";
  });
  $("#refresh-btn").addEventListener("click", refresh);
  document.addEventListener("click", async (e) => {
    if (e.target.closest("#logout-btn")) {
      await postJson("/setup/logout", {});
      showLoggedOut();
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
