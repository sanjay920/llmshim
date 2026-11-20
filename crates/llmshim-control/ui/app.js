// The admin UI (design note 0051). No framework, no build step, nothing from other origins.
// Everything shown comes from the JSON API with the person's own bearer token. Values from the
// API are put on the page as text only (never as markup), because tenant, key and model names
// are typed by callers.
"use strict";

const TOKEN_KEY = "llmshim.token";
const PKCE_KEY = "llmshim.pkce";

// ---- small helpers -------------------------------------------------------------------------

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v === null || v === undefined) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  for (const c of children.flat()) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}

const $ = (id) => document.getElementById(id);
const nf = new Intl.NumberFormat("en-US");
const fmtInt = (n) => (n === null || n === undefined ? "—" : nf.format(n));
const fmtPct = (n) => (n === null || n === undefined ? "—" : n.toFixed(n < 10 ? 1 : 0) + "%");
function fmtDur(ms) {
  if (ms === null || ms === undefined) return "—";
  return ms < 1000 ? ms + " ms" : (ms / 1000).toFixed(ms < 10000 ? 2 : 1) + " s";
}
function fmtUsd(n) {
  if (n === null || n === undefined) return "—";
  if (n === 0) return "$0.00";
  if (n < 0.01) return "$" + n.toPrecision(2);
  return "$" + n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function fmtTime(iso) {
  const d = new Date(iso);
  return isNaN(d) ? iso : d.toLocaleString(undefined, { dateStyle: "short", timeStyle: "medium" });
}
function b64url(bytes) {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
}
function randomString(n) {
  return b64url(crypto.getRandomValues(new Uint8Array(n)));
}

// A cost is exact only when the provider said so. `catalog` is an estimate from a price list and
// `provider_floor` a lower bound; an unknown cost is a dash, never zero.
function costCell(row) {
  const cost = row.cost_usd;
  if (cost === null || cost === undefined) return h("span", { class: "muted" }, "—");
  const mark =
    row.cost_source === "catalog" ? h("span", { class: "est", title: "Estimate from a price list, not the provider's bill" }, "est")
    : row.cost_source === "provider_floor" ? h("span", { class: "est", title: "A lower bound: the provider reported part of the bill" }, "≥")
    : null;
  return h("span", {}, fmtUsd(cost), mark);
}

// ---- session and API -----------------------------------------------------------------------

const session = {
  get token() { return sessionStorage.getItem(TOKEN_KEY); },
  set token(v) { v ? sessionStorage.setItem(TOKEN_KEY, v) : sessionStorage.removeItem(TOKEN_KEY); },
  who: null,
  config: null,
};

class ApiError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}

async function api(path, params, send) {
  const url = new URL(path, location.origin);
  for (const [k, v] of Object.entries(params || {})) {
    if (v !== "" && v !== null && v !== undefined) url.searchParams.set(k, v);
  }
  const init = { headers: { authorization: "Bearer " + session.token }, cache: "no-store" };
  if (send) {
    init.method = send.method;
    if (send.body !== undefined) {
      init.headers["content-type"] = "application/json";
      init.body = JSON.stringify(send.body);
    }
  }
  const response = await fetch(url, init);
  if (response.status === 401) {
    session.token = null;
    throw new ApiError(401, "Your session ended. Sign in again.");
  }
  let body = null;
  try { body = await response.json(); } catch (_) { /* not JSON */ }
  if (!response.ok) {
    throw new ApiError(response.status, (body && body.error && body.error.message) || "The request failed (" + response.status + ")");
  }
  return body;
}

// ---- sign-in -------------------------------------------------------------------------------

const redirectUri = () => location.origin + "/ui/";

async function beginOidc() {
  if (!window.crypto || !crypto.subtle) {
    renderSignIn("Sign-in through the identity provider needs a secure connection (https, or localhost). Paste a token instead.");
    return;
  }
  const oidc = session.config.oidc;
  const verifier = randomString(48);
  const state = randomString(24);
  const challenge = b64url(new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(verifier))));
  sessionStorage.setItem(PKCE_KEY, JSON.stringify({ verifier, state }));
  const url = new URL(oidc.authorization_endpoint);
  for (const [k, v] of Object.entries({
    response_type: "code", client_id: oidc.client_id, redirect_uri: redirectUri(), scope: oidc.scope,
    state, code_challenge: challenge, code_challenge_method: "S256",
  })) url.searchParams.set(k, v);
  location.assign(url);
}

// Returns an error message, or null when this is not a sign-in return.
async function finishOidc() {
  const query = new URLSearchParams(location.search);
  if (!query.has("code") && !query.has("error")) return null;
  history.replaceState(null, "", "/ui/" + location.hash);
  const saved = JSON.parse(sessionStorage.getItem(PKCE_KEY) || "null");
  sessionStorage.removeItem(PKCE_KEY);
  if (query.has("error")) return "The identity provider refused the sign-in.";
  if (!saved || query.get("state") !== saved.state) return "The sign-in could not be matched to this browser. Try again.";
  const oidc = session.config.oidc;
  let response;
  try {
    response = await fetch(oidc.token_endpoint, {
      method: "POST",
      headers: { "content-type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        grant_type: "authorization_code", code: query.get("code"), redirect_uri: redirectUri(),
        client_id: oidc.client_id, code_verifier: saved.verifier,
      }),
    });
  } catch (_) {
    return "The identity provider's token endpoint could not be reached.";
  }
  const body = await response.json().catch(() => null);
  if (!response.ok || !body || !body.access_token) return "The identity provider did not issue a token.";
  session.token = body.access_token;
  return null;
}

function renderSignIn(message) {
  $("bar").hidden = true;
  const oidc = session.config.oidc;
  const input = h("input", { type: "password", autocomplete: "off", placeholder: "Access token", "aria-label": "Access token" });
  const error = h("div", { class: "err", role: "alert" }, message || "");
  const paste = h("form", {
    class: "panel", onsubmit: async (e) => {
      e.preventDefault();
      session.token = input.value.trim();
      await start();
    },
  }, h("strong", {}, "Paste a token"), input,
    h("span", { class: "muted" }, "Held for this tab only and sent only to this server."),
    h("button", { type: "submit" }, "Use token"));
  $("main").replaceChildren(h("div", { class: "signin" },
    h("h1", {}, "llmshim control"),
    error,
    oidc ? h("div", { class: "panel" }, h("button", { class: "primary", type: "button", onclick: beginOidc }, "Sign in with your identity provider")) : null,
    session.config.break_glass || !oidc ? paste : null));
}

// ---- views ---------------------------------------------------------------------------------

const RANGES = [["24h", "Last 24 hours", 86400e3], ["7d", "Last 7 days", 7 * 86400e3], ["30d", "Last 30 days", 30 * 86400e3]];
const sinceOf = (key) => new Date(Date.now() - RANGES.find((r) => r[0] === key)[2]).toISOString();

function rangeSelect(value, onchange) {
  return h("select", { "aria-label": "Time range", onchange: (e) => onchange(e.target.value) },
    RANGES.map(([k, label]) => h("option", { value: k, selected: k === value }, label)));
}

function tile(label, value, note) {
  return h("div", { class: "tile" }, h("div", { class: "label" }, label), h("div", { class: "value" }, value),
    note ? h("div", { class: "note" }, note) : null);
}

async function viewHome() {
  let range = "7d";
  let generation = 0;
  const body = h("div");
  async function load() {
    const mine = ++generation;
    body.replaceChildren(h("p", { class: "muted" }, "Loading…"));
    const from = sinceOf(range);
    const [all, byDay, byModel] = await Promise.all([
      api("/v1/requests/summary", { from }),
      api("/v1/requests/summary", { from, group_by: "day" }),
      api("/v1/requests/summary", { from, group_by: "model" }),
    ]);
    if (mine !== generation) return;
    const t = all.totals;
    const costNote = [
      t.cost_estimated_attempts ? t.cost_estimated_attempts + " estimated or partial" : null,
      t.cost_unknown_attempts ? t.cost_unknown_attempts + " with no price" : null,
    ].filter(Boolean).join(", ");
    const days = byDay.groups.slice().sort((a, b) => String(a.group).localeCompare(String(b.group)));
    const top = Math.max(0, ...days.map((d) => d.cost_usd));
    const bars = h("div", { class: "bars" }, days.map((d) => {
      const fill = h("div", { class: "fill" });
      fill.style.width = top > 0 ? (d.cost_usd / top) * 100 + "%" : "0%";
      return h("div", { class: "bar" }, h("span", { class: "muted" }, d.group || "—"), h("div", { class: "track" }, fill),
        h("span", { class: "amount" }, fmtUsd(d.cost_usd)));
    }));
    body.replaceChildren(
      h("div", { class: "tiles" },
        tile("Spend", fmtUsd(t.cost_usd), costNote || "all from provider bills"),
        tile("Attempts", fmtInt(t.attempts), fmtInt(t.failed) + " failed"),
        tile("Tokens in / out", fmtInt(t.input_tokens) + " / " + fmtInt(t.output_tokens)),
        tile("Cache hit", fmtPct(t.cache_hit_pct), "of prompt tokens that reported it"),
        tile("Slowest", fmtDur(t.max_duration_ms), "average " + fmtDur(t.avg_duration_ms === null ? null : Math.round(t.avg_duration_ms)))),
      h("h2", {}, "Spend by day"), days.length ? bars : h("p", { class: "muted" }, "No requests in this range."),
      h("h2", {}, "By model"), groupTable(byModel.groups, "Model"),
      h("p", { class: "foot" }, "Spend adds only the costs that are known. “est” is a price-list estimate, “≥” a lower bound; attempts with no price are counted above, never added as zero."));
  }
  const view = h("div", {}, h("h1", {}, "Home"), h("div", { class: "filters" }, rangeSelect(range, (v) => { range = v; load().catch(fail); })), body);
  const fail = showErrorFor(view);
  load().catch(fail);
  return view;
}

function groupTable(groups, label) {
  if (!groups.length) return h("p", { class: "muted" }, "Nothing yet.");
  const rows = groups.slice().sort((a, b) => b.cost_usd - a.cost_usd);
  return h("div", { class: "scroll" }, h("table", {},
    h("thead", {}, h("tr", {}, h("th", {}, label), h("th", { class: "num" }, "Attempts"), h("th", { class: "num" }, "Failed"),
      h("th", { class: "num" }, "Tokens in"), h("th", { class: "num" }, "Tokens out"), h("th", { class: "num" }, "Cache hit"),
      h("th", { class: "num" }, "Cost"))),
    h("tbody", {}, rows.map((g) => h("tr", {}, h("td", {}, g.group || "—"), h("td", { class: "num" }, fmtInt(g.attempts)),
      h("td", { class: "num" }, fmtInt(g.failed)), h("td", { class: "num" }, fmtInt(g.input_tokens)),
      h("td", { class: "num" }, fmtInt(g.output_tokens)), h("td", { class: "num" }, fmtPct(g.cache_hit_pct)),
      h("td", { class: "num" }, fmtUsd(g.cost_usd), g.cost_estimated_attempts ? h("span", { class: "est" }, "est") : null))))));
}

const OUTCOMES = ["completed", "http_failure", "transport_failure", "invalid_response", "stream_failure", "abandoned"];

async function viewRequests() {
  const filters = { range: "7d", tenant: "", key: "", model: "", provider: "", outcome: "", request_id: "", min_cost_usd: "" };
  const facets = await api("/v1/requests/facets", { from: sinceOf("30d") });
  const tbody = h("tbody");
  const more = h("button", { type: "button", hidden: true }, "Load more");
  const detail = h("div");
  const status = h("p", { class: "muted" });
  let cursor = null;
  // Each reload starts a new generation; an answer from an older one is dropped, so a slow response
  // can never clear or fill the table for filters that are no longer selected.
  let generation = 0;

  const select = (name, label, values) => h("select", { "aria-label": label, onchange: (e) => { filters[name] = e.target.value; reload().catch((e) => fail(e)); } },
    h("option", { value: "" }, "Any " + label.toLowerCase()), values.map((v) => h("option", { value: v }, v)));
  const text = (name, label, width) => {
    const input = h("input", { type: "search", placeholder: label, "aria-label": label, style: false });
    input.style.width = width;
    input.addEventListener("change", () => {
      const value = input.value.trim();
      // A number is the only thing the server accepts here; say so next to the box instead of failing the view.
      const bad = name === "min_cost_usd" && value !== "" && !(Number.isFinite(Number(value)) && Number(value) >= 0);
      input.setCustomValidity(bad ? "A number of dollars, 0 or more" : "");
      input.title = bad ? "A number of dollars, 0 or more" : "";
      if (bad) { input.reportValidity(); return; }
      filters[name] = value;
      reload().catch((e) => fail(e));
    });
    return input;
  };

  function query() {
    return {
      from: sinceOf(filters.range), tenant: filters.tenant, key: filters.key, model: filters.model,
      provider: filters.provider, outcome: filters.outcome, request_id: filters.request_id,
      min_cost_usd: filters.min_cost_usd, limit: 100, cursor,
    };
  }
  function row(r) {
    const tr = h("tr", { class: "row", tabindex: "0", onclick: () => show(r), onkeydown: (e) => { if (e.key === "Enter") show(r); } },
      h("td", {}, fmtTime(r.ts)),
      h("td", {}, h("span", { class: "pill " + r.outcome }, r.outcome), r.http_status ? h("span", { class: "sub" }, " " + r.http_status) : null),
      h("td", {}, h("div", {}, r.model), h("div", { class: "sub" }, r.provider + (r.deployment ? " · " + r.deployment : ""))),
      h("td", {}, h("div", {}, r.key || "—"), h("div", { class: "sub" }, r.tenant || "—")),
      h("td", { class: "num" }, fmtInt(r.input_tokens)), h("td", { class: "num" }, fmtInt(r.output_tokens)),
      h("td", { class: "num" }, fmtPct(r.cache_hit_pct)), h("td", { class: "num" }, fmtDur(r.duration_ms)),
      h("td", { class: "num" }, costCell(r)));
    return tr;
  }
  function show(r) {
    const fields = [
      ["Attempt", r.attempt_id], ["Request", r.request_id || "—"], ["Time", r.ts], ["Outcome", r.outcome + (r.http_status ? " (" + r.http_status + ")" : "")],
      ["Provider / model", r.provider + " / " + r.model], ["Deployment", r.deployment || "—"], ["Kind", r.kind], ["Admission", r.admission],
      ["Tenant", r.tenant || "—"], ["Key", r.key || "—"], ["Duration", fmtDur(r.duration_ms)],
      ["Input tokens", fmtInt(r.input_tokens)], ["Uncached input", r.uncached_input_tokens === null ? "not reported" : fmtInt(r.uncached_input_tokens)],
      ["Cache read", fmtInt(r.cache_read_tokens)], ["Cache write", fmtInt(r.cache_write_tokens)], ["Cache hit", fmtPct(r.cache_hit_pct)],
      ["Output tokens", fmtInt(r.output_tokens)], ["Reasoning tokens", r.reasoning_tokens === null ? "—" : fmtInt(r.reasoning_tokens)],
      ["Cost", r.cost_usd === null ? "unknown" : fmtUsd(r.cost_usd)],
      ["Cost source", r.cost_source || "—"], ["Usage reported", r.usage_reported ? "yes" : "no"],
      ["Tags", r.tags ? Object.entries(r.tags).map(([k, v]) => k + "=" + v).join(", ") : "—"],
    ];
    detail.replaceChildren(h("div", { class: "panel" },
      h("div", { class: "filters" }, h("strong", {}, "Attempt detail"), h("button", { type: "button", onclick: () => detail.replaceChildren() }, "Close")),
      h("dl", {}, fields.flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, v)]))));
    detail.scrollIntoView({ block: "nearest" });
  }
  async function load(append) {
    const mine = generation;
    more.disabled = true;
    const page = await api("/v1/requests", query());
    if (mine !== generation) return;
    if (!append) tbody.replaceChildren();
    for (const r of page.requests) tbody.append(row(r));
    cursor = page.next_cursor;
    more.hidden = !cursor;
    more.disabled = false;
    status.textContent = tbody.children.length ? "" : "No requests match.";
  }
  async function reload() { generation += 1; cursor = null; detail.replaceChildren(); await load(false); }
  more.addEventListener("click", () => load(true).catch((e) => fail(e)));

  let fail = () => {};
  const view = h("div", {}, h("h1", {}, "Requests"),
    h("div", { class: "filters" },
      rangeSelect(filters.range, (v) => { filters.range = v; reload().catch((e) => fail(e)); }),
      select("tenant", "Tenant", facets.tenants || []), select("key", "Client", facets.keys || []),
      select("model", "Model", facets.models || []), select("provider", "Provider", facets.providers || []),
      select("outcome", "Outcome", OUTCOMES), text("request_id", "Request id", "190px"), text("min_cost_usd", "Min cost, USD", "130px")),
    h("div", { class: "scroll" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "Time"), h("th", {}, "Status"), h("th", {}, "Model"), h("th", {}, "Client"),
        h("th", { class: "num" }, "In"), h("th", { class: "num" }, "Out"), h("th", { class: "num" }, "Cache"),
        h("th", { class: "num" }, "Duration"), h("th", { class: "num" }, "Cost"))), tbody)),
    status, h("p", {}, more), detail,
    h("p", { class: "foot" }, "One row per provider attempt: a retry or a fallback is its own row. In counts the whole prompt including cache; Cache is the share read from cache. “est” is a price-list estimate, “≥” a lower bound, “—” an unknown cost."));
  fail = showErrorFor(view);
  reload().catch((e) => fail(e));
  return view;
}

// ---- shell ---------------------------------------------------------------------------------

const VIEWS = {
  home: ["Home", viewHome], requests: ["Requests", viewRequests], clients: ["Clients", viewClients],
  admin: ["Admin", viewAdmin], history: ["History", viewHistory],
};

// An error from a view the person has already left is not shown over the one they are on.
function showErrorFor(el) {
  return (error) => { if (el.isConnected || (error instanceof ApiError && error.status === 401)) showError(error); };
}

function showError(error) {
  if (error instanceof ApiError && error.status === 401) {
    renderSignIn(error.message);
    return;
  }
  $("main").replaceChildren(h("p", { class: "err", role: "alert" }, error.message || String(error)));
}

let routing = 0;

async function route() {
  const turn = ++routing;
  const name = (location.hash.replace(/^#\/?/, "") || "home").split("?")[0];
  const [, view] = VIEWS[name] || VIEWS.home;
  const current = VIEWS[name] ? name : "home";
  $("nav").replaceChildren(...Object.entries(VIEWS).map(([key, [label]]) =>
    h("a", { href: "#/" + key, "aria-current": key === current ? "page" : false }, label)));
  try {
    const page = await view();
    // A view that was still being built when the person moved on must not replace the one they asked for.
    if (turn === routing) $("main").replaceChildren(page);
  } catch (error) {
    if (turn === routing) showError(error);
  }
}

async function start() {
  if (!session.config) {
    session.config = await (await fetch("/ui/config.json", { cache: "no-store" })).json();
  }
  const returned = session.config.oidc ? await finishOidc() : null;
  if (!session.token) return renderSignIn(returned);
  try {
    session.who = await api("/v1/whoami");
  } catch (error) {
    return showError(error);
  }
  $("who").textContent = session.who.actor + " · " + session.who.role;
  $("bar").hidden = false;
  await route();
}

$("signout").addEventListener("click", () => { session.token = null; session.who = null; location.hash = ""; start(); });
window.addEventListener("hashchange", () => { if (session.token) route(); });
start().catch(showError);
