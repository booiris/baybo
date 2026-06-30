// remote-host operator dashboard — vanilla ES module, no bundler / framework.
//
// The bearer token lives only in localStorage and is presented solely as an
// `Authorization` header; it is never written to a cookie or logged, and key
// secrets are revealed once and dropped. All server-supplied strings reach the
// DOM through textContent (never innerHTML).

const TOKEN_KEY = "baybo.dashboard.token";
const API_BASE = new URL("api/", import.meta.url).pathname; // -> /dashboard/api/
const OVERVIEW_POLL_MS = 4000;
const RANGES = ["24h", "7d", "30d", "60d"];
const TABS = ["overview", "keys", "traffic", "devices"];

const REVEAL_FLASH_MS = 6000;
const TOP_N = 10;
const SVGNS = "http://www.w3.org/2000/svg";

// ---- token store ----

const getToken = () => localStorage.getItem(TOKEN_KEY) || "";
const setToken = (t) => localStorage.setItem(TOKEN_KEY, t);
const clearToken = () => localStorage.removeItem(TOKEN_KEY);

class Unauthorized extends Error {}

// ---- single fetch choke point ----

async function api(path, { method = "GET", body } = {}) {
  const headers = { Authorization: `Bearer ${getToken()}` };
  const opts = { method, headers };
  if (body !== undefined) {
    headers["Content-Type"] = "application/json";
    opts.body = JSON.stringify(body);
  }
  const resp = await fetch(API_BASE + path, opts);
  if (resp.status === 401) {
    clearToken();
    showLogin("Token rejected.");
    throw new Unauthorized();
  }
  if (resp.status === 204) return null;
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(text.trim() || `Request failed (${resp.status}).`);
  }
  return resp.json();
}

// ---- tiny DOM helpers ----

const $ = (sel) => document.querySelector(sel);

function el(tag, props = {}, ...kids) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(props || {})) {
    if (v == null || v === false) continue;
    if (k === "class") node.className = v;
    else if (k === "text") node.textContent = v;
    else if (k === "value") node.value = v;
    else if (k === "checked" || k === "disabled" || k === "selected") node[k] = !!v;
    else if (k.startsWith("on") && typeof v === "function") node.addEventListener(k.slice(2).toLowerCase(), v);
    else node.setAttribute(k, v === true ? "" : v);
  }
  for (const kid of kids.flat()) {
    if (kid == null || kid === false) continue;
    node.append(kid.nodeType ? kid : document.createTextNode(String(kid)));
  }
  return node;
}

function svg(tag, attrs = {}, ...kids) {
  const node = document.createElementNS(SVGNS, tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (v == null) continue;
    node.setAttribute(k, v);
  }
  for (const kid of kids.flat()) {
    if (kid == null) continue;
    node.append(kid);
  }
  return node;
}

const ICON_PATHS = {
  lock: ["M5.6 8.2V6.2a3.4 3.4 0 0 1 6.8 0v2"],
  plus: ["M9 4.2v9.6", "M4.2 9h9.6"],
  copy: ["M7 6.2h6.4a1 1 0 0 1 1 1v6.4a1 1 0 0 1-1 1H7a1 1 0 0 1-1-1V7.2a1 1 0 0 1 1-1z", "M11 6.2V4.8a1 1 0 0 0-1-1H4.6a1 1 0 0 0-1 1v6.4a1 1 0 0 0 1 1H5"],
};

function icon(name) {
  const s = svg("svg", {
    class: "icon",
    viewBox: "0 0 18 18",
    width: "16",
    height: "16",
    fill: "none",
    stroke: "currentColor",
    "stroke-width": "1.4",
    "stroke-linecap": "round",
    "stroke-linejoin": "round",
  });
  if (name === "lock") {
    s.append(svg("path", { d: ICON_PATHS.lock[0] }));
    s.append(svg("rect", { x: "4", y: "8.2", width: "10", height: "6.4", rx: "1.4" }));
    return s;
  }
  for (const d of ICON_PATHS[name] || []) s.append(svg("path", { d }));
  return s;
}

// ---- formatters ----

function fmtBytes(n) {
  n = Number(n) || 0;
  if (n < 1024) return `${n} B`;
  const units = ["KiB", "MiB", "GiB", "TiB", "PiB"];
  let v = n / 1024;
  let i = 0;
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024;
    i += 1;
  }
  const digits = v < 10 ? 2 : v < 100 ? 1 : 0;
  return `${v.toFixed(digits)} ${units[i]}`;
}

const fmtRate = (bps) => `${fmtBytes(bps)}/s`;

const fmtCount = (n) => (Number(n) || 0).toLocaleString("en-US");

function fmtAgo(secs) {
  secs = Math.max(0, Math.floor(Number(secs) || 0));
  const d = Math.floor(secs / 86400);
  const h = Math.floor((secs % 86400) / 3600);
  const m = Math.floor((secs % 3600) / 60);
  const s = secs % 60;
  if (d) return `${d}d ${h}h`;
  if (h) return `${h}h ${m}m`;
  if (m) return `${m}m ${s}s`;
  return `${s}s`;
}

const mask = (last4) => "••••" + (last4 || "");

const limitOrDash = (v, fmt) => (v == null ? "—" : fmt ? fmt(v) : String(v));

function axisTime(t) {
  const m = String(t || "").match(/^(\d{4})-(\d{2})-(\d{2})(?:[ T](\d{2}))?/);
  if (!m) return String(t || "");
  const [, , mo, day, hr] = m;
  return hr != null ? `${mo}-${day} ${hr}:00` : `${mo}-${day}`;
}

function truncLabel(s) {
  s = String(s ?? "");
  return s.length > 24 ? s.slice(0, 23) + "…" : s;
}

function shortHex(h) {
  h = String(h || "");
  return h.length > 18 ? `${h.slice(0, 8)}…${h.slice(-8)}` : h;
}

function numOrNull(v) {
  const s = (v ?? "").toString().trim();
  if (s === "") return null;
  const n = Number(s);
  return Number.isFinite(n) ? n : null;
}

// ---- toast ----

function toast(msg) {
  const t = el("div", { class: "toast", text: msg });
  $("#modal-root").append(t);
  setTimeout(() => t.remove(), 3200);
}

function errorBox(e) {
  return el("div", { class: "card empty" }, e && e.message ? e.message : "Something went wrong.");
}

// ---- modal ----

let modalKeyHandler = null;
let modalDismiss = null;

function openModal(node, onDismiss) {
  modalDismiss = onDismiss || null;
  const backdrop = el("div", { class: "modal-backdrop" });
  backdrop.addEventListener("mousedown", (e) => {
    if (e.target === backdrop) dismissModal();
  });
  backdrop.append(node);
  $("#modal-root").replaceChildren(backdrop);
  modalKeyHandler = (e) => {
    if (e.key === "Escape") dismissModal();
  };
  document.addEventListener("keydown", modalKeyHandler);
  const focusable = node.querySelector("input, select, button");
  if (focusable) focusable.focus();
}

function teardownModal() {
  if (modalKeyHandler) {
    document.removeEventListener("keydown", modalKeyHandler);
    modalKeyHandler = null;
  }
  $("#modal-root").replaceChildren();
}

function closeModal() {
  modalDismiss = null;
  teardownModal();
}

function dismissModal() {
  const d = modalDismiss;
  modalDismiss = null;
  teardownModal();
  if (d) d();
}

function field(label, control, hint) {
  return el(
    "div",
    { class: "field" },
    el("label", { class: "field-label", text: label }),
    control,
    hint ? el("div", { class: "field-hint", text: hint }) : null,
  );
}

function confirmModal({ title, desc, confirmText = "Confirm", danger = false }) {
  return new Promise((resolve) => {
    const cancel = el("button", { class: "btn btn-outline", type: "button" }, "Cancel");
    const ok = el("button", { class: danger ? "btn btn-danger" : "btn", type: "button" }, confirmText);
    cancel.addEventListener("click", () => {
      closeModal();
      resolve(false);
    });
    ok.addEventListener("click", () => {
      closeModal();
      resolve(true);
    });
    const modal = el(
      "div",
      { class: "modal" },
      el("div", { class: "modal-title", text: title }),
      el("div", { class: "modal-desc", text: desc }),
      el("div", { class: "modal-actions" }, cancel, ok),
    );
    openModal(modal, () => resolve(false));
  });
}

// ---- login / shell ----

function showLogin(message) {
  closeModal();
  stopOverviewPoll();
  const app = $("#app");
  if (app) app.hidden = true;
  const login = $("#login");
  login.hidden = false;
  const status = $("#login-status");
  if (status) {
    status.textContent = message || "";
    status.className = message ? "status err" : "status";
  }
  const tok = $("#token");
  if (tok) {
    tok.value = "";
    tok.focus();
  }
}

function wireLogin() {
  const input = $("#token");
  const btn = $("#unlock");
  const status = $("#login-status");
  const submit = async () => {
    const t = (input.value || "").trim();
    if (!t) {
      status.className = "status err";
      status.textContent = "Enter the dashboard token.";
      return;
    }
    setToken(t);
    status.className = "status";
    status.textContent = "Unlocking…";
    btn.disabled = true;
    try {
      await api("overview");
      enterApp();
    } catch (e) {
      btn.disabled = false;
      if (e instanceof Unauthorized) return; // showLogin already set the message
      clearToken();
      status.className = "status err";
      status.textContent = e.message || "Could not reach the server.";
    }
  };
  btn.addEventListener("click", submit);
  input.addEventListener("keydown", (ev) => {
    if (ev.key === "Enter") submit();
  });
}

function lock() {
  clearToken();
  showLogin("Locked.");
}

const titleCase = (t) => t.charAt(0).toUpperCase() + t.slice(1);

function renderShell() {
  const header = $("#app header");
  header.className = "app-header";
  header.replaceChildren(
    el("div", { class: "wordmark", text: "remote-host" }),
    el("button", { class: "btn btn-outline btn-sm", type: "button", onclick: lock }, icon("lock"), "Lock"),
  );
  const nav = $("#app nav.tabs");
  nav.replaceChildren(
    ...TABS.map((t) =>
      el("button", {
        class: "tab",
        type: "button",
        "data-tab": t,
        "aria-current": currentTab() === t ? "page" : null,
        text: titleCase(t),
        onclick: () => goTab(t),
      }),
    ),
  );
}

function enterApp() {
  $("#login").hidden = true;
  $("#app").hidden = false;
  renderShell();
  if (!TABS.includes(currentTab())) location.hash = "#overview";
  else route();
}

// ---- hash router ----

const currentTab = () => (location.hash || "").replace(/^#/, "") || "overview";

function goTab(t) {
  if (currentTab() === t) route();
  else location.hash = "#" + t;
}

function route() {
  stopOverviewPoll();
  let tab = currentTab();
  if (!TABS.includes(tab)) tab = "overview";
  document.querySelectorAll("#app nav.tabs .tab").forEach((b) => {
    if (b.dataset.tab === tab) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  const view = $("#view");
  view.replaceChildren();
  const render = { overview: renderOverview, keys: renderKeys, traffic: renderTraffic, devices: renderDevices }[tab];
  render(view);
}

// ---- overview (polled) ----

let overviewTimer = null;
let overviewPaint = null;

function stopOverviewPoll() {
  if (overviewTimer) {
    clearInterval(overviewTimer);
    overviewTimer = null;
  }
  overviewPaint = null;
}

async function renderOverview(view) {
  const paint = async () => {
    try {
      const snap = await api("overview");
      if (currentTab() !== "overview") return;
      view.replaceChildren(overviewView(snap));
    } catch (e) {
      if (e instanceof Unauthorized) return;
      if (currentTab() === "overview") view.replaceChildren(errorBox(e));
    }
  };
  overviewPaint = paint;
  await paint();
  if (overviewTimer) clearInterval(overviewTimer);
  overviewTimer = setInterval(() => {
    if (document.hidden) return;
    if (currentTab() !== "overview") {
      stopOverviewPoll();
      return;
    }
    paint();
  }, OVERVIEW_POLL_MS);
}

function statCard(label, value, sub) {
  return el(
    "div",
    { class: "card stat" },
    el("div", { class: "stat-label", text: label }),
    el("div", { class: "stat-value", text: value }),
    sub ? el("div", { class: "stat-sub", text: sub }) : null,
  );
}

function overviewView(s) {
  const grid = el(
    "div",
    { class: "stat-grid" },
    statCard("Keys admitted", fmtCount(s.keys_admitted)),
    statCard("Devices bound", fmtCount(s.devices_bound)),
    statCard("Push", s.push_enabled ? "enabled" : "disabled"),
    statCard("Gateways connected", fmtCount(s.gateways_connected)),
    statCard("Relay legs pending", fmtCount(s.relay_legs_pending)),
    statCard("Relay conns live", fmtCount(s.relay_conns_live)),
    statCard("Relay keys tracked", fmtCount(s.relay_keys_tracked)),
    statCard("IP entries tracked", fmtCount(s.ip_entries_tracked)),
    statCard("Uptime", fmtAgo(s.uptime_secs)),
    statCard("Build", s.build_version),
  );
  const meta = el(
    "dl",
    { class: "kv" },
    el("dt", { text: "Started at" }),
    el("dd", { text: s.started_at }),
    el("dt", { text: "Server time" }),
    el("dd", { text: s.server_time }),
  );
  return el(
    "div",
    {},
    el("div", { class: "section-title", text: "Overview" }),
    grid,
    el("div", { class: "card meta-card" }, meta),
  );
}

// ---- keys ----

async function renderKeys(view) {
  view.replaceChildren(el("div", { class: "empty", text: "Loading keys…" }));
  let rows;
  try {
    rows = await api("keys");
  } catch (e) {
    if (e instanceof Unauthorized) return;
    view.replaceChildren(errorBox(e));
    return;
  }
  if (currentTab() !== "keys") return;
  view.replaceChildren(keysView(rows));
}

function emptyRow(cols, text) {
  return el("tr", {}, el("td", { colspan: String(cols) }, el("div", { class: "empty", text })));
}

function keysView(rows) {
  const head = el(
    "div",
    { class: "view-head" },
    el("div", { class: "section-title", text: "Keys" }),
    el("button", { class: "btn", type: "button", onclick: openAdmitModal }, icon("plus"), "Admit key"),
  );
  const cols = ["Key", "Live", "Limits", "Expires", "Created", ""];
  const body = rows.length ? rows.map(keyRow) : [emptyRow(cols.length, "No keys admitted yet.")];
  const table = el(
    "table",
    { class: "table" },
    el("thead", {}, el("tr", {}, ...cols.map((h) => el("th", { text: h })))),
    el("tbody", {}, ...body),
  );
  return el("div", {}, head, el("div", { class: "card table-card" }, table));
}

function limitsCell(r) {
  const line = (k, v) =>
    [el("span", { class: "lim-k", text: k }), el("span", { class: "lim-v", text: v })];
  return el(
    "td",
    {},
    el(
      "div",
      { class: "limits" },
      ...line("conns", limitOrDash(r.max_conns)),
      ...line("rate", limitOrDash(r.max_bps, fmtRate)),
      ...line("per-srv", limitOrDash(r.per_server_max_bps, fmtRate)),
    ),
  );
}

function keyRow(r) {
  const keyCell = el(
    "td",
    {},
    el(
      "div",
      { class: "key-id" },
      el("code", { class: "key-mask", text: mask(r.key_last4) }),
      r.label ? el("span", { class: "tag", text: r.label }) : null,
      r.expired ? el("span", { class: "tag tag-expired", text: "expired" }) : null,
    ),
  );

  const reveal = el(
    "button",
    { class: "btn btn-outline btn-sm", type: "button" },
    "Reveal",
  );
  reveal.addEventListener("click", () => revealKey(r, reveal, keyCell.querySelector(".key-mask")));

  const actions = el(
    "td",
    {},
    el(
      "div",
      { class: "row-actions" },
      reveal,
      el("button", { class: "btn btn-outline btn-sm", type: "button", onclick: () => openEditModal(r) }, "Edit"),
      el("button", {
        class: "btn btn-outline btn-sm",
        type: "button",
        disabled: r.conns_live === 0,
        onclick: () => kickKey(r),
      }, "Kick"),
      el("button", { class: "btn btn-danger btn-sm", type: "button", onclick: () => revokeKey(r) }, "Revoke"),
    ),
  );

  return el(
    "tr",
    {},
    keyCell,
    el("td", { text: fmtCount(r.conns_live) }),
    limitsCell(r),
    el("td", { text: r.expires_at || "—" }),
    el("td", { text: r.created_at }),
    actions,
  );
}

async function revealKey(r, btn, cell) {
  btn.disabled = true;
  try {
    const res = await api("keys/" + r.id + "/reveal");
    let cleartext = res.remote_api_key;
    try {
      await navigator.clipboard.writeText(cleartext);
      toast("Full key copied to clipboard.");
    } catch {
      toast("Clipboard unavailable — select the shown key to copy.");
    }
    const masked = cell.textContent;
    cell.textContent = cleartext;
    cell.classList.add("cleartext");
    cleartext = null; // drop the secret from the local scope
    setTimeout(() => {
      cell.textContent = masked;
      cell.classList.remove("cleartext");
    }, REVEAL_FLASH_MS);
  } catch (e) {
    if (!(e instanceof Unauthorized)) toast(e.message);
  } finally {
    btn.disabled = false;
  }
}

async function kickKey(r) {
  const ok = await confirmModal({
    title: "Kick connections",
    desc: `Drop the live connection(s) for ${mask(r.key_last4)}. The key stays admitted; the gateway may reconnect.`,
    confirmText: "Kick",
  });
  if (!ok) return;
  try {
    const out = await api("keys/" + r.id + "/kick", { method: "POST" });
    toast(`Kicked ${fmtCount(out.kicked)} connection(s).`);
    route();
  } catch (e) {
    if (!(e instanceof Unauthorized)) toast(e.message);
  }
}

async function revokeKey(r) {
  const ok = await confirmModal({
    title: "Revoke key",
    desc: `Delete ${mask(r.key_last4)} from the allow-list. Live connections are dropped immediately.`,
    confirmText: "Revoke",
    danger: true,
  });
  if (!ok) return;
  try {
    await api("keys/" + r.id, { method: "DELETE" });
    toast("Key revoked.");
    route();
  } catch (e) {
    if (!(e instanceof Unauthorized)) toast(e.message);
  }
}

function showSecretOnce(secret) {
  const code = el("code", { class: "secret-once", text: secret });
  const copy = el("button", { class: "btn btn-outline", type: "button" }, icon("copy"), "Copy");
  copy.addEventListener("click", async () => {
    try {
      await navigator.clipboard.writeText(secret);
      copy.replaceChildren(document.createTextNode("Copied"));
    } catch {
      toast("Clipboard unavailable — select the key to copy.");
    }
  });
  const done = el("button", { class: "btn", type: "button", onclick: closeModal }, "I've saved it");
  const modal = el(
    "div",
    { class: "modal" },
    el("div", { class: "modal-title", text: "Key admitted" }),
    el("div", {
      class: "modal-desc",
      text: "Shown once. Copy it now — the full key cannot be retrieved again from this dialog.",
    }),
    el("div", { class: "secret-box" }, code),
    el("div", { class: "modal-actions" }, copy, done),
  );
  openModal(modal);
}

function openAdmitModal() {
  const status = el("div", { class: "status" });
  let generate = true;

  const keyInput = el("input", { type: "text", autocomplete: "off", placeholder: "rh_…" });
  const keyField = field("Existing key", keyInput, "Stored verbatim. Switch to Generate for a fresh key.");
  keyField.hidden = true;

  const genRadio = el("input", { type: "radio", name: "keymode", checked: true });
  const pasteRadio = el("input", { type: "radio", name: "keymode" });
  genRadio.addEventListener("change", () => {
    generate = true;
    keyField.hidden = true;
  });
  pasteRadio.addEventListener("change", () => {
    generate = false;
    keyField.hidden = false;
  });
  const radios = el(
    "div",
    { class: "radio-row" },
    el("label", {}, genRadio, "Generate"),
    el("label", {}, pasteRadio, "Paste existing"),
  );

  const labelInput = el("input", { type: "text", autocomplete: "off", placeholder: "optional" });
  const mcInput = el("input", { type: "number", min: "0", placeholder: "e.g. 4" });
  const mbInput = el("input", { type: "number", min: "0", placeholder: "bytes / second" });
  const psInput = el("input", { type: "number", min: "0", placeholder: "bytes / second (optional)" });
  const expInput = el("input", { type: "text", placeholder: "YYYY-MM-DD HH:MM:SS (UTC)" });
  const expField = field("Expires at", expInput, "Blank = never.");

  const submit = el("button", { class: "btn", type: "button" }, "Admit");

  const sync = () => {
    submit.disabled = !String(mcInput.value).trim() || !String(mbInput.value).trim();
  };
  mcInput.addEventListener("input", sync);
  mbInput.addEventListener("input", sync);

  submit.addEventListener("click", async () => {
    const body = {
      label: labelInput.value.trim() || null,
      max_conns: numOrNull(mcInput.value),
      max_bps: numOrNull(mbInput.value),
      per_server_max_bps: numOrNull(psInput.value),
      expires_at: expInput.value.trim() || null,
    };
    if (!generate) {
      const k = keyInput.value.trim();
      if (!k) {
        status.className = "status err";
        status.textContent = "Paste a key or switch to Generate.";
        return;
      }
      body.key = k; // Generate omits `key` entirely
    }
    submit.disabled = true;
    status.className = "status";
    status.textContent = "Admitting…";
    try {
      const out = await api("keys", { method: "POST", body });
      closeModal();
      route();
      showSecretOnce(out.remote_api_key);
    } catch (e) {
      if (e instanceof Unauthorized) return;
      status.className = "status err";
      status.textContent = e.message;
      submit.disabled = false;
    }
  });

  const modal = el(
    "div",
    { class: "modal" },
    el("div", { class: "modal-title", text: "Admit key" }),
    el("div", { class: "modal-desc", text: "Add a remote API key to the allow-list." }),
    radios,
    keyField,
    field("Label", labelInput),
    field("Max conns", mcInput),
    field("Max rate (bytes/s)", mbInput),
    field("Per-server rate (bytes/s)", psInput),
    expField,
    status,
    el(
      "div",
      { class: "modal-actions" },
      el("button", { class: "btn btn-outline", type: "button", onclick: closeModal }, "Cancel"),
      submit,
    ),
  );
  openModal(modal);
  sync();
}

function openEditModal(r) {
  const status = el("div", { class: "status" });
  const labelInput = el("input", { type: "text", autocomplete: "off", value: r.label || "" });
  const mcInput = el("input", { type: "number", min: "0", value: r.max_conns ?? "" });
  const mbInput = el("input", { type: "number", min: "0", value: r.max_bps ?? "" });
  const psInput = el("input", { type: "number", min: "0", value: r.per_server_max_bps ?? "" });
  const expInput = el("input", { type: "text", value: r.expires_at || "", placeholder: "YYYY-MM-DD HH:MM:SS (UTC)" });
  const submit = el("button", { class: "btn", type: "button" }, "Save");

  const sync = () => {
    submit.disabled = !String(mcInput.value).trim() || !String(mbInput.value).trim();
  };
  mcInput.addEventListener("input", sync);
  mbInput.addEventListener("input", sync);

  submit.addEventListener("click", async () => {
    const body = {
      label: labelInput.value.trim() || null,
      max_conns: numOrNull(mcInput.value),
      max_bps: numOrNull(mbInput.value),
      per_server_max_bps: numOrNull(psInput.value),
      expires_at: expInput.value.trim() || null,
    };
    submit.disabled = true;
    status.className = "status";
    status.textContent = "Saving…";
    try {
      await api("keys/" + r.id, { method: "PATCH", body });
      closeModal();
      toast("Key updated.");
      route();
    } catch (e) {
      if (e instanceof Unauthorized) return;
      status.className = "status err";
      status.textContent = e.message;
      submit.disabled = false;
    }
  });

  const modal = el(
    "div",
    { class: "modal" },
    el("div", { class: "modal-title", text: "Edit key" }),
    el("div", { class: "modal-desc" }, el("code", { text: mask(r.key_last4) })),
    field("Label", labelInput),
    field("Max conns", mcInput),
    field("Max rate (bytes/s)", mbInput),
    field("Per-server rate (bytes/s)", psInput),
    field("Expires at", expInput, "Blank = never. Back-dating expires the key on reload."),
    status,
    el(
      "div",
      { class: "modal-actions" },
      el("button", { class: "btn btn-outline", type: "button", onclick: closeModal }, "Cancel"),
      submit,
    ),
  );
  openModal(modal);
  sync();
}

// ---- charts (hand-rolled inline SVG) ----

function areaChart(buckets, { series, fmt }) {
  const W = 640;
  const H = 180;
  const padL = 12;
  const padR = 12;
  const padT = 16;
  const padB = 24;
  const innerW = W - padL - padR;
  const innerH = H - padT - padB;
  const wrap = el("div", { class: "chart-wrap" });
  if (!buckets.length) {
    wrap.append(el("div", { class: "empty small", text: "No data in range." }));
    return wrap;
  }

  let max = 0;
  for (const b of buckets) for (const s of series) max = Math.max(max, Number(b[s.key]) || 0);
  if (max <= 0) max = 1;

  const n = buckets.length;
  const x = (i) => padL + (n === 1 ? innerW / 2 : (i / (n - 1)) * innerW);
  const y = (v) => padT + innerH - (Math.max(0, Number(v) || 0) / max) * innerH;
  const base = padT + innerH;

  const root = svg("svg", { class: "chart-svg", viewBox: `0 0 ${W} ${H}`, role: "img" });
  for (let g = 0; g <= 3; g += 1) {
    const gy = padT + (innerH * g) / 3;
    root.append(svg("line", { class: "grid-line", x1: padL, y1: gy, x2: W - padR, y2: gy }));
  }

  series.forEach((s, si) => {
    const pts = buckets.map((b, i) => [x(i), y(b[s.key])]);
    const line = pts.map((p, i) => `${i ? "L" : "M"}${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ");
    const area =
      `M${x(0).toFixed(1)} ${base.toFixed(1)} ` +
      pts.map((p) => `L${p[0].toFixed(1)} ${p[1].toFixed(1)}`).join(" ") +
      ` L${x(n - 1).toFixed(1)} ${base.toFixed(1)} Z`;
    root.append(svg("path", { class: si ? "area area-2" : "area", d: area }));
    root.append(svg("path", { class: si ? "line line-2" : "line", d: line }));
  });

  root.append(svg("text", { class: "axis", x: padL, y: padT - 5, "text-anchor": "start" }, fmt(max)));
  root.append(svg("text", { class: "axis", x: padL, y: H - 6, "text-anchor": "start" }, axisTime(buckets[0].t)));
  root.append(svg("text", { class: "axis", x: W - padR, y: H - 6, "text-anchor": "end" }, axisTime(buckets[n - 1].t)));

  const cursor = svg("line", { class: "cursor-line", x1: -10, x2: -10, y1: padT, y2: base, opacity: "0" });
  root.append(cursor);
  const overlay = svg("rect", { x: padL, y: padT, width: innerW, height: innerH, fill: "transparent" });
  root.append(overlay);

  const tip = el("div", { class: "chart-tooltip" });
  wrap.append(root, tip);

  root.addEventListener("pointermove", (ev) => {
    const rect = root.getBoundingClientRect();
    if (!rect.width) return;
    const px = ((ev.clientX - rect.left) / rect.width) * W;
    let i = Math.round(((px - padL) / innerW) * (n - 1));
    i = Math.max(0, Math.min(n - 1, i));
    const cx = x(i);
    cursor.setAttribute("x1", cx);
    cursor.setAttribute("x2", cx);
    cursor.setAttribute("opacity", "1");
    const parts = series.map((s) => `${s.label} ${fmt(buckets[i][s.key])}`).join("  ·  ");
    tip.textContent = `${axisTime(buckets[i].t)} — ${parts}`;
    const leftPx = Math.max(0, Math.min(rect.width, (cx / W) * rect.width));
    tip.style.left = `${leftPx}px`;
    tip.style.opacity = "1";
  });
  root.addEventListener("pointerleave", () => {
    tip.style.opacity = "0";
    cursor.setAttribute("opacity", "0");
  });

  return wrap;
}

function barChart(items, { labelOf, valueOf, fmt }) {
  const wrap = el("div", { class: "chart-wrap" });
  const shown = items.slice(0, TOP_N);
  const extra = items.length - shown.length;
  if (!shown.length) {
    wrap.append(el("div", { class: "empty small", text: "No data in range." }));
    return wrap;
  }

  const W = 640;
  const rowH = 26;
  const padT = 4;
  const padB = 4;
  const labelW = 150;
  const valW = 96;
  const H = padT + padB + shown.length * rowH;
  const trackX = labelW;
  const trackW = W - labelW - valW;
  const max = Math.max(1, ...shown.map((it) => Number(valueOf(it)) || 0));

  const root = svg("svg", { class: "chart-svg bar-svg", viewBox: `0 0 ${W} ${H}`, role: "img" });
  shown.forEach((it, i) => {
    const cy = padT + i * rowH;
    const mid = cy + rowH / 2 + 4;
    const v = Number(valueOf(it)) || 0;
    const bw = Math.max(2, (v / max) * trackW);
    root.append(svg("text", { class: "bar-label-svg", x: 0, y: mid }, truncLabel(labelOf(it))));
    root.append(svg("rect", { class: "bar-track-svg", x: trackX, y: cy + 5, width: trackW, height: rowH - 12, rx: 7 }));
    root.append(svg("rect", { class: "bar-fill-svg", x: trackX, y: cy + 5, width: bw.toFixed(1), height: rowH - 12, rx: 7 }));
    root.append(svg("text", { class: "bar-val-svg", x: W, y: mid, "text-anchor": "end" }, fmt(v)));
  });

  wrap.append(root);
  if (extra > 0) wrap.append(el("div", { class: "bar-more muted small", text: `+${extra} more` }));
  return wrap;
}

function chartHead(title, sub) {
  return el(
    "div",
    { class: "chart-head" },
    el("div", { class: "chart-title", text: title }),
    el("div", { class: "chart-sub muted", text: sub }),
  );
}

const subhead = (text) => el("div", { class: "subhead", text });

// ---- traffic ----

let trafficRange = "24h";

async function renderTraffic(view) {
  const body = el("div", { class: "traffic-body" }, el("div", { class: "empty", text: "Loading…" }));
  const pills = el(
    "div",
    { class: "pills" },
    ...RANGES.map((rg) =>
      el("button", {
        class: "pill",
        type: "button",
        "data-range": rg,
        "aria-pressed": rg === trafficRange ? "true" : "false",
        text: rg,
        onclick: () => {
          if (trafficRange === rg) return;
          trafficRange = rg;
          pills.querySelectorAll(".pill").forEach((p) =>
            p.setAttribute("aria-pressed", p.dataset.range === trafficRange ? "true" : "false"),
          );
          loadTraffic(body);
        },
      }),
    ),
  );
  view.replaceChildren(
    el("div", { class: "view-head" }, el("div", { class: "section-title", text: "Traffic" }), pills),
    body,
  );
  await loadTraffic(body);
}

async function loadTraffic(body) {
  body.replaceChildren(el("div", { class: "empty", text: "Loading…" }));
  const r = trafficRange;
  try {
    const [relay, push, ip] = await Promise.all([
      api("traffic/relay?range=" + r),
      api("traffic/push?range=" + r),
      api("traffic/ip?range=" + r),
    ]);
    if (currentTab() !== "traffic" || trafficRange !== r) return;
    body.replaceChildren(trafficView(relay, push, ip));
  } catch (e) {
    if (e instanceof Unauthorized) return;
    body.replaceChildren(errorBox(e));
  }
}

function trafficView(relay, push, ip) {
  const relayCard = el(
    "div",
    { class: "card chart-card" },
    chartHead(
      "Relay",
      `${fmtBytes(relay.totals.bytes_up + relay.totals.bytes_down)} · ${fmtCount(relay.totals.frames_up + relay.totals.frames_down)} frames`,
    ),
    areaChart(relay.buckets, {
      series: [
        { key: "bytes_up", label: "up" },
        { key: "bytes_down", label: "down" },
      ],
      fmt: fmtBytes,
    }),
    subhead("Top keys"),
    barChart(relay.by_key, { labelOf: (x) => mask(x.key_last4), valueOf: (x) => x.bytes_up + x.bytes_down, fmt: fmtBytes }),
  );

  const pushCard = el(
    "div",
    { class: "card chart-card" },
    chartHead("Push", `${fmtCount(push.totals.sends)} sends · ${fmtBytes(push.totals.bytes)}`),
    areaChart(push.buckets, { series: [{ key: "bytes", label: "bytes" }], fmt: fmtBytes }),
    subhead("Top devices"),
    barChart(push.by_device, { labelOf: (x) => x.device_id, valueOf: (x) => x.bytes, fmt: fmtBytes }),
  );

  const ipCard = el(
    "div",
    { class: "card chart-card" },
    chartHead("IP", `${fmtCount(ip.totals.requests)} requests · ${fmtBytes(ip.totals.bytes)}`),
    areaChart(ip.buckets, { series: [{ key: "bytes", label: "bytes" }], fmt: fmtBytes }),
    subhead("Top sources"),
    barChart(ip.by_ip, { labelOf: (x) => x.ip, valueOf: (x) => x.bytes, fmt: fmtBytes }),
  );

  return el("div", { class: "grid-3" }, relayCard, pushCard, ipCard);
}

// ---- devices & IPs ----

async function renderDevices(view) {
  view.replaceChildren(el("div", { class: "empty", text: "Loading…" }));
  try {
    const [devices, ip] = await Promise.all([api("devices"), api("traffic/ip?range=" + trafficRange)]);
    if (currentTab() !== "devices") return;
    view.replaceChildren(devicesView(devices, ip));
  } catch (e) {
    if (e instanceof Unauthorized) return;
    view.replaceChildren(errorBox(e));
  }
}

function deviceRow(d) {
  return el(
    "tr",
    {},
    el("td", {}, el("code", { text: d.device_id })),
    el("td", {}, el("span", { class: "tag", text: d.apns_env })),
    el("td", {}, el("code", { text: d.token_masked })),
    el("td", {}, el("code", { class: "pubkey", text: shortHex(d.gateway_pubkey_hex) })),
    el("td", { class: "num", text: fmtCount(d.last_counter) }),
    el("td", { class: "num", text: fmtCount(d.sends_total) }),
    el("td", { class: "num", text: fmtBytes(d.bytes_total) }),
  );
}

function ipTable(headers, rows, render) {
  const body = rows.length ? rows.map(render) : [emptyRow(headers.length, "No traffic in range.")];
  return el(
    "table",
    { class: "table" },
    el("thead", {}, el("tr", {}, ...headers.map((h, i) => el("th", { class: i ? "num" : null, text: h })))),
    el("tbody", {}, ...body),
  );
}

function devicesView(devices, ip) {
  const devCols = ["Device", "APNs env", "Token", "Gateway pubkey", "Counter", "Sends", "Bytes"];
  const devBody = devices.length ? devices.map(deviceRow) : [emptyRow(devCols.length, "No devices bound.")];
  const devTable = el(
    "table",
    { class: "table" },
    el(
      "thead",
      {},
      el("tr", {}, ...devCols.map((h, i) => el("th", { class: i >= 4 ? "num" : null, text: h }))),
    ),
    el("tbody", {}, ...devBody),
  );

  const byIp = ipTable(["Source IP", "Requests", "Bytes"], ip.by_ip, (r) =>
    el(
      "tr",
      {},
      el("td", {}, el("code", { text: r.ip })),
      el("td", { class: "num", text: fmtCount(r.requests) }),
      el("td", { class: "num", text: fmtBytes(r.bytes) }),
    ),
  );
  const byEndpoint = ipTable(["Endpoint", "Requests", "Bytes"], ip.by_endpoint, (r) =>
    el(
      "tr",
      {},
      el("td", {}, el("code", { text: r.endpoint })),
      el("td", { class: "num", text: fmtCount(r.requests) }),
      el("td", { class: "num", text: fmtBytes(r.bytes) }),
    ),
  );

  return el(
    "div",
    {},
    el("div", { class: "section-title", text: "Devices" }),
    el("div", { class: "card table-card" }, devTable),
    el("div", { class: "section-title", text: `IP traffic · ${trafficRange}` }),
    el(
      "div",
      { class: "grid-2" },
      el(
        "div",
        { class: "card" },
        subhead("By source IP"),
        barChart(ip.by_ip, { labelOf: (x) => x.ip, valueOf: (x) => x.bytes, fmt: fmtBytes }),
        byIp,
      ),
      el(
        "div",
        { class: "card" },
        subhead("By endpoint"),
        barChart(ip.by_endpoint, { labelOf: (x) => x.endpoint, valueOf: (x) => x.bytes, fmt: fmtBytes }),
        byEndpoint,
      ),
    ),
  );
}

// ---- boot ----

function boot() {
  wireLogin();
  window.addEventListener("hashchange", () => {
    if (!$("#app").hidden) route();
  });
  document.addEventListener("visibilitychange", () => {
    if (!document.hidden && overviewPaint && !$("#app").hidden && currentTab() === "overview") overviewPaint();
  });
  if (getToken()) enterApp();
  else showLogin();
}

boot();
