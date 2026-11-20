// The admin UI's shell, sign-in, Home and Requests (design note 0051). Everything shown comes from the JSON
// API with the person's own bearer token. kit.js (helpers, charts) loads first; manage.js (the management
// views) loads before this file and uses its helpers only when a view is opened.
"use strict";

const TOKEN_KEY = "llmshim.token";
const PKCE_KEY = "llmshim.pkce";

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

const b64url = (bytes) => btoa(String.fromCharCode(...bytes)).replace(/\+/g, "-").replace(/\//g, "_").replace(/=+$/, "");
const randomString = (n) => b64url(crypto.getRandomValues(new Uint8Array(n)));
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

// Returns an error message, or null when this is not a sign-in return (or it worked).
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
  document.title = "Sign in · llmshim control";
  const oidc = session.config.oidc;
  const input = h("input", { class: "input", type: "password", autocomplete: "off", spellcheck: "false", placeholder: "Paste an access token", "aria-label": "Access token" });
  const error = h("div", { class: "err", role: "alert" }, message || "");
  const paste = h("form", {
    class: "form", onsubmit: async (e) => {
      e.preventDefault();
      const value = input.value.trim();
      if (!value) { error.textContent = "Paste a token first."; return; }
      session.token = value;
      await start(true);
    },
  }, h("label", { class: "field" }, h("span", {}, "Access token"), input, h("small", {}, "Held for this tab only and sent only to this server.")),
    h("button", { class: "btn primary", type: "submit" }, "Continue"));
  const showPaste = session.config.break_glass || !oidc;
  $("root").replaceChildren(h("div", { class: "signin-page" }, h("div", { class: "signin" },
    h("div", { class: "brand" }, h("div", { class: "brand-mark" }, brandMark())),
    h("h1", {}, "Sign in to llmshim control"),
    h("p", { class: "lead" }, "Usage, clients and gateways in one place."),
    h("div", { class: "card" },
      error,
      oidc ? h("button", { class: "btn primary", type: "button", onclick: beginOidc }, "Continue with your identity provider") : null,
      oidc && showPaste ? h("div", { class: "or" }, "or") : null,
      showPaste ? paste : null))));
  input.focus();
}

// ---- shell ---------------------------------------------------------------------------------

const NAV = [
  ["Observe", [["home", "Home", "home"], ["requests", "Requests", "requests"]]],
  ["Manage", [["clients", "Clients", "clients"], ["admin", "Organizations", "admin"], ["gateways", "Gateways", "gateways"], ["history", "History", "history"]]],
];
const VIEWS = {
  home: viewHome, requests: viewRequests, clients: viewClients, admin: viewAdmin, gateways: viewGateways, history: viewHistory,
};
const TITLES = { home: "Home", requests: "Requests", clients: "Clients", admin: "Organizations", gateways: "Gateways", history: "History" };

function renderShell() {
  const nav = h("nav", { class: "nav", "aria-label": "Sections" }, NAV.map(([group, items]) => h("div", {}, h("div", { class: "nav-label" }, group),
    items.map(([key, label, ic]) => h("a", { href: "#/" + key, "data-key": key }, icon(ic, 16), label)))));
  const who = session.who || {};
  $("root").replaceChildren(h("div", { class: "app" },
    h("aside", { class: "side" },
      h("div", { class: "brand" }, h("div", { class: "brand-mark" }, brandMark()), h("div", {}, h("b", {}, "llmshim"), h("span", {}, "control"))),
      nav,
      h("div", { class: "side-foot" }, h("div", { class: "avatar" }, initial(who.actor)),
        h("div", { class: "who" }, h("b", { title: who.actor }, String(who.actor || "").replace(/^oidc:/, "")), h("span", {}, String(who.role || "no role").replace("-", " "))),
        h("button", { class: "btn icon ghost", type: "button", title: "Sign out", "aria-label": "Sign out", onclick: signOut }, icon("logout", 16)))),
    h("div", { class: "content" }, h("header", { class: "top", id: "top" }), h("main", { id: "main", tabindex: "-1" }))));
  const top = $("top");
  window.addEventListener("scroll", () => top.classList.toggle("scrolled", window.scrollY > 4), { passive: true });
}

function setHeader(title, sub, actions) {
  const top = $("top");
  if (!top) return;
  top.replaceChildren(h("div", { class: "grow" }, h("h1", {}, title), sub ? h("div", { class: "sub" }, sub) : null), h("div", { class: "actions" }, actions || null));
}

function signOut() {
  session.token = null; session.who = null; location.hash = "";
  start();
}

function showError(error) {
  if (error instanceof ApiError && error.status === 401) {
    renderSignIn(error.message);
    return;
  }
  const main = $("main");
  if (!main) return;
  main.replaceChildren(h("div", { class: "card" }, emptyState({
    icon: "alert", title: "This page could not be loaded", text: error.message || String(error),
    action: h("button", { class: "btn", type: "button", onclick: () => route() }, icon("refresh", 14), "Try again"),
  })));
}
// An error from a view the person has already left is not shown over the one they are on.
function showErrorFor(el) {
  return (error) => { if (el.isConnected || (error instanceof ApiError && error.status === 401)) showError(error); };
}

let routing = 0;
async function route() {
  const turn = ++routing;
  const name = (location.hash.replace(/^#\/?/, "") || "home").split("?")[0];
  const key = VIEWS[name] ? name : "home";
  document.title = TITLES[key] + " · llmshim control";
  document.querySelectorAll(".nav a").forEach((a) => (a.dataset.key === key ? a.setAttribute("aria-current", "page") : a.removeAttribute("aria-current")));
  const ctx = { turn, header: (...args) => { if (turn === routing) setHeader(...args); } };
  ctx.header(TITLES[key]);
  const main = $("main");
  main.replaceChildren(skeleton(6));
  window.scrollTo(0, 0);
  try {
    const page = await VIEWS[key](ctx);
    if (turn === routing) main.replaceChildren(page);
  } catch (error) {
    if (turn === routing) showError(error);
  }
}

async function start(pasted) {
  if (!session.config) {
    session.config = await (await fetch("/ui/config.json", { cache: "no-store" })).json();
  }
  const returned = session.config.oidc ? await finishOidc() : null;
  if (!session.token) return renderSignIn(returned);
  try {
    session.who = await api("/v1/whoami");
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) return renderSignIn(pasted ? "That token was not accepted. Check it and try again." : error.message);
    return showError(error);
  }
  renderShell();
  await route();
}

window.addEventListener("hashchange", () => { if (session.token && $("main")) route(); });
start().catch((error) => {
  $("root").replaceChildren(h("div", { class: "signin-page" }, h("div", { class: "signin" }, h("div", { class: "card" }, h("div", { class: "err" }, "The page could not start: " + (error.message || error))))));
});

// ---- ranges, buckets -----------------------------------------------------------------------

const RANGES = [["24h", "24 hours"], ["7d", "7 days"], ["30d", "30 days"]];
const HOUR = 3600e3, DAY = 86400e3;
const startOfUtcDay = (t) => Math.floor(t / DAY) * DAY;
const dayKey = (t) => new Date(t).toISOString().slice(0, 10);

// The window a range names: hourly buckets for 24 hours (the current hour is the last), whole UTC days
// for 7 and 30 (today is the last, partial). `prev` is the window of the same length just before it.
function windowOf(range) {
  const now = Date.now();
  if (range === "24h") {
    const from = Math.floor(now / HOUR) * HOUR - 23 * HOUR;
    return { mode: "hour", from, prevFrom: from - 24 * HOUR, label: "24 hours", buckets: Array.from({ length: 24 }, (_, i) => ({ key: String(from / HOUR + i), t: from + i * HOUR })) };
  }
  const days = range === "30d" ? 30 : 7;
  const from = startOfUtcDay(now) - (days - 1) * DAY;
  return { mode: "day", from, prevFrom: from - days * DAY, label: days + " days", buckets: Array.from({ length: days }, (_, i) => ({ key: dayKey(from + i * DAY), t: from + i * DAY })) };
}
function bucketLabel(win, t) {
  const d = new Date(t);
  return win.mode === "hour"
    ? { short: d.toLocaleTimeString(undefined, { hour: "numeric" }), full: d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric" }) }
    : { short: d.toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" }), full: d.toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric", timeZone: "UTC" }) };
}
const iso = (t) => new Date(t).toISOString();

// ---- Home ----------------------------------------------------------------------------------

let homeRange = "7d";

async function viewHome(ctx) {
  const page = h("div", { class: "stack" });
  const fail = showErrorFor(page);
  const body = h("div", { class: "stack" }, skeleton(8));
  let generation = 0;

  async function load() {
    const mine = ++generation;
    const win = windowOf(homeRange);
    const from = iso(win.from);
    const [cur, series, prev, byModel, byKey] = await Promise.all([
      api("/v1/requests/summary", { from }),
      api("/v1/requests/summary", { from, group_by: win.mode }),
      api("/v1/requests/summary", { from: iso(win.prevFrom), to: from }),
      api("/v1/requests/summary", { from, group_by: "model" }),
      api("/v1/requests/summary", { from, group_by: "key" }),
    ]);
    if (mine !== generation) return;
    body.replaceChildren(...homeBody(win, cur.totals, series.groups, prev.totals, byModel.groups, byKey.groups));
  }
  ctx.header("Home", "Spend, cache and reliability across every gateway", segmented(RANGES, homeRange, (v) => { homeRange = v; load().catch(fail); }));
  page.append(body);
  await load();
  return page;
}

function homeBody(win, t, groups, prev, models, keys) {
  if (!t.attempts) {
    return [h("div", { class: "card" }, emptyState({
      icon: "inbox", title: "No requests in the last " + win.label,
      text: "Gateways send their usage here when [usage_export] is set in their configuration. Once a request completes it appears within a couple of seconds.",
    }))];
  }
  const byBucket = new Map(groups.map((g) => [g.group, g]));
  const rows = win.buckets.map((b) => ({ b, g: byBucket.get(b.key) || null, ...bucketLabel(win, b.t) }));
  const costRows = rows.map((r) => {
    const g = r.g;
    return {
      label: r.full, short: r.short, value: g && g.cost_usd > 0 ? g.cost_usd : 0,
      tipRows: g ? [
        { label: "Spend", value: g.cost_usd > 0 || g.cost_unknown_attempts < g.attempts ? fmtUsd(g.cost_usd) : "no price" },
        { label: "Requests", value: fmtInt(g.attempts) },
        ...(g.failed ? [{ label: "Failed", value: fmtInt(g.failed) }] : []),
        ...(g.cost_estimated_attempts ? [{ label: "Estimated or partial", value: fmtInt(g.cost_estimated_attempts) }] : []),
        ...(g.cost_unknown_attempts ? [{ label: "No price", value: fmtInt(g.cost_unknown_attempts) }] : []),
      ] : [{ label: "Requests", value: "0" }],
    };
  });
  const cacheRows = rows.map((r) => ({
    label: r.full, short: r.short, value: r.g && r.g.cache_hit_pct !== null ? r.g.cache_hit_pct : null,
    tipRows: r.g ? [{ key: true, label: "Cache hit", value: fmtPct(r.g.cache_hit_pct) }, { label: "Cached tokens", value: fmtCompact(r.g.cache_read_tokens) }, { label: "Prompt tokens", value: fmtCompact(r.g.input_tokens) }] : [{ label: "Requests", value: "0" }],
  }));

  // The hero: spend, with its change on the window before. Costs that are unknown are said, never added as zero.
  const delta = prev.cost_usd > 0 && t.cost_usd > 0 ? (t.cost_usd - prev.cost_usd) / prev.cost_usd : null;
  const notes = [];
  if (t.cost_estimated_attempts) notes.push(fmtInt(t.cost_estimated_attempts) + " estimated or partial");
  if (t.cost_unknown_attempts) notes.push(fmtInt(t.cost_unknown_attempts) + " with no price");
  const hero = h("section", { class: "card" },
    h("div", { class: "card-head" }, h("div", { class: "grow" }, h("h2", {}, "Spend"), h("div", { class: "sub" }, "Last " + win.label + (win.mode === "day" ? " · days are UTC" : ""))),
      h("button", { class: "btn small ghost", type: "button", "aria-pressed": "false", onclick: toggleTable }, "Table")),
    h("div", { class: "card-body" },
      h("div", { class: "hero" }, fmtUsd(t.cost_usd)),
      h("div", { class: "hero-note" }, delta === null ? "No earlier period to compare" : h("span", { class: "delta" }, icon(delta >= 0 ? "arrow" : "arrowdown", 14), (delta >= 0 ? "+" : "−") + Math.abs(Math.round(delta * 100)) + "% vs the previous " + win.label), notes.length ? h("span", { class: "muted dotsep" }, "· " + notes.join(", ")) : null),
      h("div", { id: "spend-view" }, columnChart({ data: costRows, fmtAxis: fmtUsdAxis, height: 290, ariaLabel: "Spend per " + win.mode })),
    ));
  function toggleTable(e) {
    const btn = e.currentTarget, view = hero.querySelector("#spend-view");
    const on = btn.getAttribute("aria-pressed") !== "true";
    btn.setAttribute("aria-pressed", String(on)); btn.textContent = on ? "Chart" : "Table";
    view.replaceChildren(on ? h("div", { class: "chart-table" }, tableOf(["Period", "Spend", "Requests", "Failed"], rows.map((r) => h("tr", {}, h("td", {}, r.full), h("td", { class: "num" }, r.g ? fmtUsd(r.g.cost_usd) : "—"), h("td", { class: "num" }, fmtInt(r.g ? r.g.attempts : 0)), h("td", { class: "num" }, fmtInt(r.g ? r.g.failed : 0)))), ["l", "r", "r", "r"])) : columnChart({ data: costRows, fmtAxis: fmtUsdAxis, height: 290, ariaLabel: "Spend per " + win.mode }));
  }

  const failRate = t.attempts ? (t.failed * 100) / t.attempts : 0;
  const tiles = h("div", { class: "tiles" },
    tile("Requests", fmtCompact(t.attempts), h("span", {}, h("i", { class: "dot " + (t.failed ? "bad" : "good") }), " " + fmtInt(t.failed) + " failed" + (t.failed ? " (" + fmtPct(failRate) + ")" : "")), sparkline(rows.map((r) => (r.g ? r.g.attempts : 0)))),
    tile("Cache hit rate", fmtPct(t.cache_hit_pct), fmtCompact(t.cache_read_tokens) + " tokens read from cache", sparkline(rows.map((r) => (r.g ? r.g.cache_hit_pct : null)))),
    tile("Tokens", fmtCompact(t.input_tokens) + " in", fmtCompact(t.output_tokens) + " out", null),
    tile("Latency", fmtDur(t.avg_duration_ms === null ? null : Math.round(t.avg_duration_ms)), "average · slowest " + fmtDur(t.max_duration_ms), null));

  const cacheCard = chartCard({
    title: "Cache hit rate", sub: "Share of prompt tokens served from cache",
    chart: lineChart({ data: cacheRows, max: 100, fmtAxis: (v) => Math.round(v) + "%", ariaLabel: "Cache hit rate per " + win.mode }),
    columns: ["Period", "Cache hit", "Cached tokens"],
    rows: rows.map((r) => [r.full, r.g ? fmtPct(r.g.cache_hit_pct) : "—", r.g ? fmtInt(r.g.cache_read_tokens) : "—"]),
  });
  const modelsSorted = models.slice().sort((a, b) => b.cost_usd - a.cost_usd || b.attempts - a.attempts);
  const spendByModel = card("Spend by model", "Ranked, with how much of each prompt was cached",
    h("div", {}, rankBars(modelsSorted.slice(0, 7).map((g) => ({ name: g.group || "unknown", sub: g.cache_hit_pct === null ? "" : fmtPct(g.cache_hit_pct) + " cached", display: g.cost_usd > 0 || g.cost_unknown_attempts < g.attempts ? fmtUsd(g.cost_usd) : "no price", value: g.cost_usd }))),
      modelsSorted.length > 7 ? h("p", { class: "foot" }, "and " + (modelsSorted.length - 7) + " more in the table below") : null));

  const keysSorted = keys.slice().sort((a, b) => b.cost_usd - a.cost_usd || b.attempts - a.attempts);
  const clients = card("Clients", "By spend", keysSorted.length ? tableOf(["Client", "Requests", "Failed", "Cache hit", "Spend"], keysSorted.slice(0, 8).map((g) => h("tr", {},
    h("td", {}, g.group || h("span", { class: "muted" }, "unknown")), h("td", { class: "num" }, fmtInt(g.attempts)), h("td", { class: "num" }, fmtInt(g.failed)),
    h("td", { class: "num" }, fmtPct(g.cache_hit_pct)), h("td", { class: "num" }, fmtUsd(g.cost_usd), g.cost_estimated_attempts ? h("span", { class: "est" }, "est") : null))), ["l", "r", "r", "r", "r"]) : h("p", { class: "muted" }, "No clients yet."));
  const composition = card("Prompt composition", "Where each model's input tokens came from",
    h("div", {}, h("div", { class: "gap-top" }, legend([["a", "Uncached"], ["b", "Read from cache"], ["c", "Written to cache"]])), h("div", { class: "table-wrap" }, tableOf(["Model", "Prompt", "Mix", "Cache hit"], modelsSorted.slice(0, 8).map((g) => {
      const uncached = Math.max(0, g.input_tokens - g.cache_read_tokens - g.cache_write_tokens);
      return h("tr", {}, h("td", {}, g.group || "unknown"), h("td", { class: "num" }, fmtCompact(g.input_tokens)),
        h("td", {}, segBar([{ name: "Uncached", value: uncached, cls: "a" }, { name: "Read from cache", value: g.cache_read_tokens, cls: "b" }, { name: "Written to cache", value: g.cache_write_tokens, cls: "c" }])),
        h("td", { class: "num" }, fmtPct(g.cache_hit_pct)));
    }), ["l", "r", "l", "r"]))));

  return [
    h("div", { class: "grid-2" }, hero, tiles),
    h("div", { class: "grid-2" }, cacheCard, spendByModel),
    h("div", { class: "grid-eq" }, clients, composition),
    h("p", { class: "foot" }, "Spend adds only the costs that are known: “est” is a price-list estimate and “≥” a lower bound, and requests with no price are counted, never added as zero. A cache hit is the share of prompt tokens the provider read from its cache, over requests that reported it."),
  ];
}

function tile(label, value, note, spark) {
  return h("div", { class: "card tile" }, h("div", { class: "label" }, label), h("div", { class: "row" }, h("div", {}, h("div", { class: "value" }, value), h("div", { class: "note" }, note)), spark || null));
}

// ---- Requests ------------------------------------------------------------------------------

const OUTCOMES = [["completed", "Completed"], ["http_failure", "HTTP failure"], ["transport_failure", "Transport failure"], ["invalid_response", "Invalid response"], ["stream_failure", "Stream failure"], ["abandoned", "Abandoned"]];
let requestFilters = { range: "7d", tenant: "", key: "", model: "", provider: "", outcome: "", request_id: "", min_cost_usd: "" };

async function viewRequests(ctx) {
  const filters = requestFilters;
  const facets = await api("/v1/requests/facets", { from: iso(Date.now() - 30 * DAY) });
  const tbody = h("tbody");
  const more = h("button", { type: "button", class: "btn", hidden: true }, "Load more");
  const count = h("span");
  const empty = h("div", { hidden: true });
  const toolbar = h("div", { class: "toolbar" });
  let cursor = null, generation = 0, shown = 0;
  const page = h("div", {});
  const fail = showErrorFor(page);

  const select = (name, label, values) => {
    const el = h("select", { class: "select" + (filters[name] ? " on" : ""), "aria-label": label, onchange: (e) => { filters[name] = e.target.value; el.classList.toggle("on", !!filters[name]); changed(); } },
      h("option", { value: "" }, label), values.map((v) => (Array.isArray(v) ? h("option", { value: v[0], selected: filters[name] === v[0] }, v[1]) : h("option", { value: v, selected: filters[name] === v }, v))));
    return el;
  };
  const search = h("input", { class: "input", type: "search", placeholder: "Request id", "aria-label": "Request id", value: filters.request_id });
  search.addEventListener("change", () => { filters.request_id = search.value.trim(); changed(); });
  const minCost = h("input", { class: "input", type: "text", inputmode: "decimal", placeholder: "Min cost, $", "aria-label": "Minimum cost in dollars", value: filters.min_cost_usd });
  minCost.style.width = "112px";
  minCost.addEventListener("change", () => {
    const v = minCost.value.trim();
    const bad = v !== "" && !(Number.isFinite(Number(v)) && Number(v) >= 0);
    minCost.setCustomValidity(bad ? "A number of dollars, 0 or more" : "");
    if (bad) { minCost.reportValidity(); return; }
    filters.min_cost_usd = v; changed();
  });
  const clear = h("button", { type: "button", class: "btn ghost small", hidden: true, onclick: () => { Object.assign(filters, { tenant: "", key: "", model: "", provider: "", outcome: "", request_id: "", min_cost_usd: "" }); build(); changed(); } }, icon("x", 12), "Clear filters");

  function build() {
    toolbar.replaceChildren(
      select("tenant", "Tenant", facets.tenants || []), select("key", "Client", facets.keys || []), select("model", "Model", facets.models || []),
      select("provider", "Provider", facets.providers || []), select("outcome", "Outcome", OUTCOMES), minCost,
      h("span", { class: "search" }, icon("search", 14), search), clear);
    search.value = filters.request_id; minCost.value = filters.min_cost_usd;
    clear.hidden = !["tenant", "key", "model", "provider", "outcome", "request_id", "min_cost_usd"].some((k) => filters[k]);
  }
  const since = () => (filters.range === "24h" ? Date.now() - DAY : filters.range === "30d" ? Date.now() - 30 * DAY : Date.now() - 7 * DAY);
  function query() {
    return { from: iso(since()), tenant: filters.tenant, key: filters.key, model: filters.model, provider: filters.provider, outcome: filters.outcome, request_id: filters.request_id, min_cost_usd: filters.min_cost_usd, limit: 100, cursor };
  }
  function row(r) {
    const open = () => showRequest(r);
    return h("tr", { class: "row", tabindex: "0", onclick: open, onkeydown: (e) => { if (e.key === "Enter") open(); } },
      h("td", { title: r.ts }, h("div", { class: "two" }, ago(r.ts), h("small", {}, fmtTime(r.ts)))),
      h("td", {}, statusPill(r.outcome)),
      h("td", {}, h("div", { class: "two" }, r.model, h("small", {}, r.provider + (r.deployment ? " · " + r.deployment : "")))),
      h("td", {}, h("div", { class: "two" }, r.key || "—", h("small", {}, r.tenant || "no tenant"))),
      h("td", { class: "num" }, fmtCompact(r.input_tokens), h("span", { class: "muted" }, " → "), fmtCompact(r.output_tokens)),
      h("td", { class: "num" }, r.cache_hit_pct === null ? h("span", { class: "muted" }, "—") : cacheMini(r.cache_hit_pct)),
      h("td", { class: "num" }, fmtDur(r.duration_ms)),
      h("td", { class: "num" }, costCell(r)));
  }
  async function load(append) {
    const mine = generation;
    more.disabled = true;
    const result = await api("/v1/requests", query());
    if (mine !== generation) return;
    if (!append) { tbody.replaceChildren(); shown = 0; }
    for (const r of result.requests) tbody.append(row(r));
    shown += result.requests.length;
    cursor = result.next_cursor;
    more.hidden = !cursor; more.disabled = false;
    count.textContent = shown ? "Showing " + fmtInt(shown) + (cursor ? " so far" : "") : "";
    empty.hidden = shown > 0;
    empty.replaceChildren(shown ? "" : emptyState({ icon: "search", title: "No requests match", text: "Try a wider time range or clear a filter." }));
  }
  function changed() { generation += 1; cursor = null; clear.hidden = !["tenant", "key", "model", "provider", "outcome", "request_id", "min_cost_usd"].some((k) => filters[k]); load(false).catch(fail); }
  more.addEventListener("click", () => load(true).catch(fail));

  ctx.header("Requests", "One row per provider attempt: a retry or a fallback is its own row", segmented(RANGES, filters.range, (v) => { filters.range = v; changed(); }));
  build();
  page.append(toolbar,
    h("div", { class: "card" }, h("div", { class: "table-wrap" }, h("table", {},
      h("thead", {}, h("tr", {}, h("th", {}, "Time"), h("th", {}, "Status"), h("th", {}, "Model"), h("th", {}, "Client"), h("th", { class: "num" }, "Tokens in → out"), h("th", { class: "num" }, "Cache hit"), h("th", { class: "num" }, "Duration"), h("th", { class: "num" }, "Cost"))), tbody)),
      empty, h("div", { class: "table-foot" }, count, more)),
    h("p", { class: "foot" }, "Tokens in count the whole prompt, cache included. Cache hit is the share of the prompt read from cache. “est” marks a price-list estimate, “≥” a lower bound, and “—” an unknown cost: never zero."));
  await load(false);
  return page;
}

function cacheMini(pct) {
  const fill = h("span");
  fill.style.width = Math.min(100, pct) + "%";
  return h("div", { class: "cell-cache" }, h("div", { class: "meter" }, fill), h("span", {}, fmtPct(pct)));
}

function showRequest(r) {
  const uncached = r.uncached_input_tokens === null ? null : r.uncached_input_tokens;
  const prompt = uncached === null
    ? h("p", { class: "muted" }, "The provider did not report how much of the prompt was cached, so no split is shown.")
    : h("div", {}, segBar([{ name: "Uncached", value: uncached, cls: "a" }, { name: "Read from cache", value: r.cache_read_tokens, cls: "b" }, { name: "Written to cache", value: r.cache_write_tokens, cls: "c" }]),
      h("div", { class: "section-gap" }), legend([["a", "Uncached " + fmtInt(uncached)], ["b", "Read from cache " + fmtInt(r.cache_read_tokens)], ["c", "Written " + fmtInt(r.cache_write_tokens)]]));
  const id = (text, what) => h("span", { class: "copy-row" }, h("span", { class: "mono" }, text || "—"), text ? copyButton(text, what) : null);
  const tags = r.tags && Object.keys(r.tags).length ? Object.entries(r.tags).map(([k, v]) => k + " = " + v).join(", ") : "—";
  openSheet({
    title: r.model, subtitle: r.provider + (r.deployment ? " · " + r.deployment : "") + " · " + fmtTime(r.ts),
    body: [
      h("div", {}, h("div", { class: "hero" }, r.cost_usd === null ? "—" : fmtUsd(r.cost_usd)), h("div", { class: "note ink2" }, r.cost_usd === null ? "No price: the provider reported usage the price list could not price" : r.cost_source === "provider" ? "The provider's own bill" : r.cost_source === "provider_floor" ? "A lower bound: the provider reported part of the bill" : "An estimate from a price list, not the provider's bill"), h("div", { class: "sub-row" }, statusPill(r.outcome), r.http_status ? h("span", { class: "muted" }, " HTTP " + r.http_status) : null)),
      h("div", { class: "section" }, h("h3", {}, "Prompt tokens"), prompt),
      h("div", { class: "section" }, h("h3", {}, "Usage"), h("dl", { class: "kv" },
        h("dt", {}, "Input"), h("dd", {}, fmtInt(r.input_tokens)), h("dt", {}, "Output"), h("dd", {}, fmtInt(r.output_tokens)),
        h("dt", {}, "Reasoning"), h("dd", {}, r.reasoning_tokens === null ? "—" : fmtInt(r.reasoning_tokens)), h("dt", {}, "Cache hit"), h("dd", {}, fmtPct(r.cache_hit_pct)),
        h("dt", {}, "Duration"), h("dd", {}, fmtDur(r.duration_ms)), h("dt", {}, "Kind"), h("dd", {}, r.kind), h("dt", {}, "Usage reported"), h("dd", {}, r.usage_reported ? "yes" : "no"))),
      h("div", { class: "section" }, h("h3", {}, "Who"), h("dl", { class: "kv" },
        h("dt", {}, "Client"), h("dd", {}, r.key || "—"), h("dt", {}, "Tenant"), h("dd", {}, r.tenant || "—"), h("dt", {}, "Tags"), h("dd", {}, tags), h("dt", {}, "Admission"), h("dd", {}, r.admission))),
      h("div", { class: "section" }, h("h3", {}, "Identifiers"), h("dl", { class: "kv" },
        h("dt", {}, "Request"), h("dd", {}, id(r.request_id, "request id")), h("dt", {}, "Attempt"), h("dd", {}, id(r.attempt_id, "attempt id")), h("dt", {}, "Time"), h("dd", { class: "m" }, r.ts))),
    ],
  });
}
