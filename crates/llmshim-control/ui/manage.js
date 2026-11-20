// The management views (design note 0051): Clients, Organizations, Gateways and History. They load before
// app.js and use its helpers (api, ApiError, session, showError, showErrorFor, setHeader) only when a view is
// opened. As everywhere in the UI, values from the API are put on the page as text, never as markup. The
// controls a role may not use are not shown; the API refuses them in any case.
"use strict";

const canIssue = () => ["admin", "key-issuer"].includes(session.who && session.who.role);
const canAdmin = () => (session.who && session.who.role) === "admin";
const send = (method, path, body) => api(path, null, { method, body });
const arr = (x) => (Array.isArray(x) ? x : []);
const enc = encodeURIComponent;

// ---- form helpers --------------------------------------------------------------------------

// Parse a form field: blank is `null` (no value / clear), otherwise a whole number at or above `min`.
function wholeNumber(text, label, min) {
  const value = text.trim();
  if (value === "") return null;
  const n = Number(value);
  if (!Number.isInteger(n) || n < min) throw new Error(label + " must be a whole number, " + min + " or more");
  return n;
}
function decimal(text, label) {
  const value = text.trim();
  if (value === "") return null;
  const n = Number(value);
  if (!Number.isFinite(n) || n <= 0) throw new Error(label + " must be a number above 0");
  return n;
}
function expiry(text) {
  if (text === "") return null;
  const d = new Date(text);
  if (isNaN(d)) throw new Error("Expiry must be a date and time");
  return d.toISOString().replace(/\.\d{3}Z$/, "Z");
}
// The value a datetime-local input takes for an instant: the person's own clock, to the minute.
function localInput(iso) {
  const d = new Date(iso);
  if (isNaN(d)) return "";
  const pad = (n) => String(n).padStart(2, "0");
  return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + "T" + pad(d.getHours()) + ":" + pad(d.getMinutes());
}
const list = (text) => text.split(",").map((x) => x.trim()).filter(Boolean);
const field = (label, input, hint) => h("label", { class: "field" }, h("span", {}, label), input, hint ? h("small", {}, hint) : null);
const input = (attrs) => h("input", { class: "input", type: "text", autocomplete: "off", spellcheck: "false", ...attrs });
const errorBox = () => h("div", { class: "err", role: "alert" });

// Run a form's action with its submit button off (a double click is one request), show a failure beside the
// form, and say whether it worked.
async function submit(event, box, action) {
  event.preventDefault();
  const button = event.submitter;
  if (button && button.disabled) return false;
  if (button) button.disabled = true;
  box.textContent = "";
  try {
    await action();
    return true;
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) { showError(error); return false; }
    box.textContent = error.message || String(error);
    return false;
  } finally {
    if (button) button.disabled = false;
  }
}
// A button that asks twice before it does something that cannot be undone.
function confirmButton(label, what, action, cls) {
  const button = h("button", { type: "button", class: "btn " + (cls || "") }, label);
  let armed = false, timer = null;
  const reset = () => { armed = false; clearTimeout(timer); button.textContent = label; button.classList.remove("danger"); };
  button.addEventListener("click", async () => {
    if (!armed) {
      armed = true; button.textContent = "Confirm: " + what; button.classList.add("danger");
      timer = setTimeout(reset, 6000);
      return;
    }
    reset(); button.disabled = true;
    try { await action(); } finally { button.disabled = false; }
  });
  return button;
}
// A secret is shown here once and nowhere else: the server does not keep it and the page does not either.
function showSecret(title, subtitle, secret) {
  const box = input({ readonly: true, value: secret, "aria-label": "Secret", class: "input secret" });
  box.addEventListener("focus", () => box.select());
  const sheet = openSheet({
    title, subtitle, sticky: true,
    body: [
      h("div", { class: "notice" }, icon("key", 16), h("div", {}, h("b", {}, "Copy this secret now."), h("div", { class: "muted" }, "It is shown once. The server stores only a hash, so it cannot be shown again."))),
      box,
      h("div", { class: "form-actions" }, h("button", { type: "button", class: "btn", onclick: () => copyText(secret, "Secret") }, icon("copy", 14), "Copy"),
        h("button", { type: "button", class: "btn primary", onclick: () => sheet.close(true) }, "I have stored it")),
    ],
  });
  box.select();
  return sheet;
}
function inlineAdd(label, placeholder, action) {
  const slot = h("span", { class: "add-inline" });
  const show = () => {
    const field = input({ placeholder, "aria-label": placeholder, maxlength: "128", required: true, class: "input" });
    const box = errorBox();
    slot.replaceChildren(h("form", { class: "add-inline", onsubmit: async (e) => { if (await submit(e, box, () => action(field.value.trim()))) { /* the view reloads */ } } },
      field, h("button", { class: "btn primary small", type: "submit" }, "Add"), h("button", { class: "btn ghost small", type: "button", onclick: rest }, "Cancel"), box));
    field.focus();
  };
  const rest = () => slot.replaceChildren(h("button", { class: "btn ghost small", type: "button", onclick: show }, icon("plus", 12), label));
  rest();
  return slot;
}
const budgetText = (usd, window) => {
  if (usd === null || usd === undefined) return "No budget";
  const w = !window ? "" : window % 86400 === 0 ? " per " + (window / 86400 === 1 ? "day" : window / 86400 + " days") : window % 3600 === 0 ? " per " + window / 3600 + " h" : " per " + window + " s";
  return fmtUsd(usd) + w;
};
const limitsText = (k) => {
  const parts = [k.rpm !== null && k.rpm !== undefined ? fmtCompact(k.rpm) + " req/min" : null, k.tpm !== null && k.tpm !== undefined ? fmtCompact(k.tpm) + " tok/min" : null].filter(Boolean);
  return parts.length ? parts.join(" · ") : "No rate limit";
};

// ---- Clients -------------------------------------------------------------------------------

let clientOrg = null;

async function viewClients(ctx) {
  const orgs = arr((await api("/v1/orgs")).orgs);
  if (!orgs.length) {
    ctx.header("Clients");
    return h("div", { class: "card" }, emptyState({ icon: "clients", title: "No organization yet", text: "Clients belong to a project, and projects to a team in an organization." + (canAdmin() ? " Create the organization first." : ""),
      action: canAdmin() ? h("a", { class: "btn primary", href: "#/admin" }, "Go to organizations") : null }));
  }
  if (!orgs.some((o) => o.id === clientOrg)) clientOrg = orgs[0].id;
  const page = h("div", { class: "stack" });
  const fail = showErrorFor(page);
  const body = h("div", {});
  let projects = new Map(), keys = [], generation = 0;

  async function load() {
    const mine = ++generation;
    const [teams, found] = await Promise.all([api("/v1/orgs/" + enc(clientOrg) + "/teams"), api("/v1/orgs/" + enc(clientOrg) + "/keys")]);
    const perTeam = await Promise.all(arr(teams.teams).map(async (t) => [t, arr((await api("/v1/teams/" + enc(t.id) + "/projects")).projects)]));
    if (mine !== generation) return;
    projects = new Map();
    for (const [t, ps] of perTeam) for (const p of ps) projects.set(p.id, { label: t.name + " / " + p.name, team: t.name, project: p.name });
    keys = arr(found.keys);
    render();
  }
  function render() {
    if (!keys.length) {
      body.replaceChildren(h("div", { class: "card" }, emptyState({ icon: "key", title: "No clients in this organization", text: "A client is a caller key: it names who is calling, which models it may use and how much it may spend.",
        action: canIssue() ? h("button", { class: "btn primary", type: "button", onclick: newClient }, icon("plus", 14), "New client") : null })));
      return;
    }
    const rows = keys.map((k) => h("tr", { class: "row", tabindex: "0", onclick: () => showClient(k), onkeydown: (e) => { if (e.key === "Enter") showClient(k); } },
      h("td", {}, h("div", { class: "two" }, k.name, h("small", {}, k.kind.replace("_", " ")))),
      h("td", {}, (projects.get(k.project_id) || { label: k.project_id }).label),
      h("td", {}, h("span", { class: "pill" }, h("i", { class: "dot " + (k.state === "active" ? "good" : "bad") }), k.state === "active" ? (k.expires_at && new Date(k.expires_at) < new Date() ? "Expired" : "Active") : "Revoked")),
      h("td", {}, limitsText(k)), h("td", {}, budgetText(k.budget_usd, k.budget_window_secs)),
      h("td", { title: k.created_at }, ago(k.created_at))));
    body.replaceChildren(h("div", { class: "card" }, tableOf(["Client", "Project", "Status", "Rate limit", "Budget", "Created"], rows)),
      h("p", { class: "foot" }, "Revoking or changing a client takes effect when the gateways take their next policy snapshot."));
  }

  function showClient(k) {
    const where = (projects.get(k.project_id) || { label: k.project_id }).label;
    let sheet;
    const refresh = async () => { await load().catch(fail); };
    const body = [h("div", { class: "sub-row" }, h("span", { class: "pill" }, h("i", { class: "dot " + (k.state === "active" ? "good" : "bad") }), k.state === "active" ? "Active" : "Revoked since " + fmtTime(k.revoked_at)))];
    body.push(h("div", { class: "section" }, h("h3", {}, "Limits"), h("dl", { class: "kv" },
      h("dt", {}, "Rate limit"), h("dd", {}, limitsText(k)), h("dt", {}, "Budget"), h("dd", {}, budgetText(k.budget_usd, k.budget_window_secs) + (k.budget_allow_unpriced ? " · unpriced requests allowed" : "")),
      h("dt", {}, "Expires"), h("dd", {}, k.expires_at ? fmtTime(k.expires_at) : "Never"), h("dt", {}, "Priority tier"), h("dd", {}, String(k.tier)))));
    body.push(h("div", { class: "section" }, h("h3", {}, "Access"), h("dl", { class: "kv" },
      h("dt", {}, "Models"), h("dd", {}, k.allowed_models ? k.allowed_models.join(", ") : "Any"), h("dt", {}, "Addresses"), h("dd", {}, k.allowed_cidrs ? k.allowed_cidrs.join(", ") : "Any"),
      h("dt", {}, "Credential"), h("dd", {}, k.kind.replace("_", " ") + (k.client_san ? " · " + k.client_san : "") + (k.jwt_table ? " · " + k.jwt_table + " / " + k.jwt_subject : "")),
      h("dt", {}, "Tags"), h("dd", {}, Object.keys(k.tags || {}).length ? Object.entries(k.tags).map(([a, b]) => a + " = " + b).join(", ") : "—"))));
    if (canIssue() && k.state === "active") {
      body.push(h("div", { class: "section" }, h("h3", {}, "Edit limits"), limitsForm(k, async () => { sheet.close(true); toast("Limits saved"); await refresh(); })));
      body.push(h("div", { class: "section" }, h("h3", {}, "Rotate"), rotateForm(k, async (made) => { sheet.close(true); await refresh(); showSecret("New secret for " + made.key.name, where, made.secret); })));
      body.push(h("div", { class: "section" }, h("h3", {}, "Revoke"), h("p", { class: "muted gap-bottom" }, "Stops this client at the next snapshot. It cannot be undone; issue a new client instead."),
        confirmButton("Revoke client", "revoke " + k.name, async () => {
          try { await send("POST", "/v1/keys/" + enc(k.id) + "/revoke"); sheet.close(true); toast(k.name + " revoked"); await refresh(); } catch (e) { toast(e.message, "bad"); }
        })));
    } else if (!canIssue()) {
      body.push(h("p", { class: "muted" }, "Your role can read clients but not change them."));
    }
    body.push(h("div", { class: "section" }, h("h3", {}, "Identifier"), h("span", { class: "copy-row" }, h("span", { class: "mono" }, k.id), copyButton(k.id, "client id"))));
    sheet = openSheet({ title: k.name, subtitle: where, body });
  }

  function limitsForm(k, done) {
    const rpm = input({ value: k.rpm ?? "", inputmode: "numeric", placeholder: "No limit" }), tpm = input({ value: k.tpm ?? "", inputmode: "numeric", placeholder: "No limit" });
    const budget = input({ value: k.budget_usd ?? "", inputmode: "decimal", placeholder: "No budget" }), window = input({ value: k.budget_window_secs ?? "", inputmode: "numeric", placeholder: "e.g. 86400" });
    const until = input({ type: "datetime-local", value: k.expires_at ? localInput(k.expires_at) : "" });
    const box = errorBox();
    return h("form", { class: "form", onsubmit: async (e) => {
      if (await submit(e, box, () => send("PATCH", "/v1/keys/" + enc(k.id) + "/limits", {
        rpm: wholeNumber(rpm.value, "Requests per minute", 1), tpm: wholeNumber(tpm.value, "Tokens per minute", 1), budget_usd: decimal(budget.value, "Budget"),
        budget_window_secs: wholeNumber(window.value, "Budget window", 1), expires_at: expiry(until.value) }))) await done();
    } },
      h("div", { class: "form-row" }, field("Requests per minute", rpm), field("Tokens per minute", tpm)),
      h("div", { class: "form-row" }, field("Budget, USD", budget), field("Budget window, seconds", window)),
      field("Expires (your time)", until, "Leave a field blank to clear that limit."), box,
      h("div", { class: "form-actions" }, h("button", { class: "btn primary", type: "submit" }, "Save limits")));
  }
  function rotateForm(k, done) {
    const name = input({ value: k.name + "-next", maxlength: "128", "aria-label": "Name of the replacement" });
    const box = errorBox();
    return h("form", { class: "form", onsubmit: async (e) => {
      let made = null;
      if (await submit(e, box, async () => { made = await send("POST", "/v1/keys/" + enc(k.id) + "/rotate", { name: name.value.trim() }); })) await done(made);
    } }, field("Name of the replacement", name, "Issues a new secret with the same limits. This client stays active until you revoke it."), box,
      h("div", { class: "form-actions" }, h("button", { class: "btn", type: "submit" }, "Rotate")));
  }

  function newClient() {
    if (!projects.size) { toast("Create a team and a project first", "bad"); return; }
    const project = h("select", { class: "select", "aria-label": "Project" }, [...projects].map(([id, p]) => h("option", { value: id }, p.label)));
    const name = input({ maxlength: "128", required: true, placeholder: "e.g. checkout-prod" });
    const kind = h("select", { class: "select", "aria-label": "Credential" }, [["secret", "Secret (bearer token)"], ["client_san", "Client certificate"], ["jwt", "Workload token"]].map(([v, l]) => h("option", { value: v }, l)));
    const san = input({ placeholder: "dns:name or uri:…" }), table = input({ placeholder: "issuer table" }), subject = input({ placeholder: "exact sub" });
    const extra = h("div", { class: "form" });
    const syncKind = () => extra.replaceChildren(...(kind.value === "client_san" ? [field("Certificate identity", san)] : kind.value === "jwt" ? [field("Issuer table", table), field("Subject", subject)] : []));
    kind.addEventListener("change", syncKind); syncKind();
    const tier = input({ value: "0", inputmode: "numeric" }), rpm = input({ inputmode: "numeric", placeholder: "No limit" }), tpm = input({ inputmode: "numeric", placeholder: "No limit" });
    const budget = input({ inputmode: "decimal", placeholder: "No budget" }), window = input({ inputmode: "numeric", placeholder: "e.g. 86400" });
    const models = input({ placeholder: "openai/gpt-6-luna, anthropic/*" }), cidrs = input({ placeholder: "10.0.0.0/8" }), until = input({ type: "datetime-local" });
    const box = errorBox();
    let sheet;
    const form = h("form", { class: "form", onsubmit: async (e) => {
      let made = null;
      if (await submit(e, box, async () => {
        made = await send("POST", "/v1/projects/" + enc(project.value) + "/keys", {
          name: name.value.trim(), tier: wholeNumber(tier.value, "Tier", 0) ?? 0, rpm: wholeNumber(rpm.value, "Requests per minute", 1), tpm: wholeNumber(tpm.value, "Tokens per minute", 1),
          budget_usd: decimal(budget.value, "Budget"), budget_window_secs: wholeNumber(window.value, "Budget window", 1),
          allowed_models: list(models.value).length ? list(models.value) : null, allowed_cidrs: list(cidrs.value).length ? list(cidrs.value) : null, expires_at: expiry(until.value),
          credential: kind.value === "client_san" ? { type: "client_san", san: san.value.trim() } : kind.value === "jwt" ? { type: "jwt", table: table.value.trim(), subject: subject.value.trim() } : { type: "secret" } });
      })) {
        sheet.close(true); await load().catch(fail);
        if (made.secret) showSecret("Secret for " + made.key.name, "Copy it now", made.secret); else toast(made.key.name + " created");
      }
    } },
      field("Project", project), field("Name", name), field("Credential", kind), extra,
      h("div", { class: "form-row" }, field("Requests per minute", rpm), field("Tokens per minute", tpm)),
      h("div", { class: "form-row" }, field("Budget, USD", budget), field("Budget window, seconds", window)),
      field("Allowed models", models, "Comma-separated provider/model, provider/* or route/name. Blank allows any."),
      field("Allowed addresses", cidrs, "Comma-separated CIDR ranges. Blank allows any."),
      h("div", { class: "form-row" }, field("Priority tier", tier, "Higher is queued first."), field("Expires (your time)", until)), box,
      h("div", { class: "form-actions" }, h("button", { class: "btn", type: "button", onclick: () => sheet.close(true) }, "Cancel"), h("button", { class: "btn primary", type: "submit" }, "Create client")));
    sheet = openSheet({ title: "New client", subtitle: "Issues a credential for a project", body: [form] });
    name.focus();
  }

  const orgSelect = orgs.length > 1 ? h("select", { class: "select", "aria-label": "Organization", onchange: (e) => { clientOrg = e.target.value; load().catch(fail); } }, orgs.map((o) => h("option", { value: o.id, selected: o.id === clientOrg }, o.name))) : null;
  ctx.header("Clients", orgs.length > 1 ? "Caller keys in the selected organization" : orgs[0].name + " · caller keys",
    [orgSelect, canIssue() ? h("button", { class: "btn primary", type: "button", onclick: newClient }, icon("plus", 14), "New client") : null]);
  page.append(body);
  await load();
  return page;
}

// ---- Organizations -------------------------------------------------------------------------

async function viewAdmin(ctx) {
  const page = h("div", { class: "stack" });
  const fail = showErrorFor(page);

  async function load() {
    const orgs = arr((await api("/v1/orgs")).orgs);
    const trees = await Promise.all(orgs.map(async (o) => {
      const teams = arr((await api("/v1/orgs/" + enc(o.id) + "/teams")).teams);
      return [o, await Promise.all(teams.map(async (t) => [t, arr((await api("/v1/teams/" + enc(t.id) + "/projects")).projects)]))];
    }));
    page.replaceChildren(...(trees.length ? trees.map(([o, teams]) => orgCard(o, teams)) : [h("div", { class: "card" }, emptyState({ icon: "admin", title: "No organizations yet", text: canAdmin() ? "An organization holds teams, teams hold projects, and projects hold clients." : "An administrator creates organizations.",
      action: canAdmin() ? h("button", { class: "btn primary", type: "button", onclick: newOrg }, icon("plus", 14), "New organization") : null }))]));
  }
  const reload = () => load().catch(fail);

  function orgCard(o, teams) {
    return h("section", { class: "card tree-org" },
      h("header", {}, h("div", { class: "grow" }, h("h2", {}, o.name), h("span", { class: "copy-row muted" }, h("span", { class: "mono" }, o.id), copyButton(o.id, "organization id"))),
        canAdmin() ? inlineAdd("Team", "Team name", async (name) => { await send("POST", "/v1/orgs/" + enc(o.id) + "/teams", { name }); toast("Team created"); await reload(); }) : null),
      teams.length ? teams.map(([t, projects]) => teamRow(t, projects)) : h("div", { class: "team" }, h("span", { class: "muted" }, "No teams yet.")));
  }
  function teamRow(t, projects) {
    return h("div", { class: "team" },
      h("div", { class: "team-head" }, h("b", {}, t.name), h("span", { class: "pill" }, icon("shield", 12), t.budget ? budgetText(t.budget.budget_usd, t.budget.budget_window_secs) : "No team budget"),
        canAdmin() ? h("button", { class: "btn ghost small", type: "button", onclick: () => budgetSheet(t) }, t.budget ? "Edit budget" : "Set budget") : null),
      h("div", { class: "chips" }, projects.map((p) => h("span", { class: "chip", title: p.id }, p.name)), canAdmin() ? inlineAdd("Project", "Project name", async (name) => { await send("POST", "/v1/teams/" + enc(t.id) + "/projects", { name }); toast("Project created"); await reload(); }) : null));
  }
  function budgetSheet(t) {
    const usd = input({ value: t.budget ? t.budget.budget_usd : "", inputmode: "decimal", placeholder: "e.g. 500" });
    const window = input({ value: t.budget && t.budget.budget_window_secs ? t.budget.budget_window_secs : "", inputmode: "numeric", placeholder: "e.g. 2592000 for 30 days" });
    const unpriced = h("input", { type: "checkbox", checked: !!(t.budget && t.budget.budget_allow_unpriced) });
    const box = errorBox();
    let sheet;
    const form = h("form", { class: "form", onsubmit: async (e) => {
      if (await submit(e, box, async () => {
        const amount = decimal(usd.value, "Budget");
        if (amount === null) throw new Error("Enter a budget, or clear it");
        await send("PUT", "/v1/teams/" + enc(t.id) + "/budget", { budget_usd: amount, budget_window_secs: wholeNumber(window.value, "Window", 1), budget_allow_unpriced: unpriced.checked });
      })) { sheet.close(true); toast("Budget saved"); await reload(); }
    } },
      field("Budget, USD", usd), field("Window, seconds", window, "Spend counts against the budget over this window."),
      h("label", { class: "field" }, h("span", {}, "Unpriced requests"), h("span", { class: "copy-row" }, unpriced, h("span", { class: "muted" }, "Allow requests whose cost the price list cannot compute"))), box,
      h("div", { class: "form-actions" }, t.budget ? confirmButton("Clear budget", "clear it", async () => {
        try { await send("DELETE", "/v1/teams/" + enc(t.id) + "/budget"); sheet.close(true); toast("Budget cleared"); await reload(); } catch (e) { box.textContent = e.message; }
      }) : null, h("button", { class: "btn primary", type: "submit" }, "Save budget")));
    sheet = openSheet({ title: t.name + " budget", subtitle: "Spend across every client of the team", body: [form] });
    usd.focus();
  }
  function newOrg() {
    const name = input({ maxlength: "128", required: true, placeholder: "Organization name" });
    const box = errorBox();
    let sheet;
    sheet = openSheet({ title: "New organization", body: [h("form", { class: "form", onsubmit: async (e) => {
      if (await submit(e, box, () => send("POST", "/v1/orgs", { name: name.value.trim() }))) { sheet.close(true); toast("Organization created"); await reload(); }
    } }, field("Name", name), box, h("div", { class: "form-actions" }, h("button", { class: "btn primary", type: "submit" }, "Create organization")))] });
    name.focus();
  }

  ctx.header("Organizations", "Organizations, teams, projects and team budgets", canAdmin() ? h("button", { class: "btn primary", type: "button", onclick: newOrg }, icon("plus", 14), "New organization") : null);
  await load();
  return page;
}

// ---- History -------------------------------------------------------------------------------

let historyLimit = 100;

async function viewHistory(ctx) {
  const page = h("div", { class: "stack" });
  const fail = showErrorFor(page);
  const result = h("div", {});
  const body = h("div", {});

  async function load() {
    const rows = arr((await api("/v1/history", { limit: historyLimit })).history);
    if (!rows.length) { body.replaceChildren(h("div", { class: "card" }, emptyState({ icon: "history", title: "No changes recorded", text: "Creating an organization, a team, a project or a client appears here with who did it." }))); return; }
    body.replaceChildren(h("div", { class: "card" }, tableOf(["When", "Who", "Action", "Subject", "Detail"], rows.map((r) => {
      const detail = r.detail === null || r.detail === undefined ? "" : JSON.stringify(r.detail);
      return h("tr", { class: "row", tabindex: "0", onclick: () => showChange(r), onkeydown: (e) => { if (e.key === "Enter") showChange(r); } },
        h("td", { title: r.at }, ago(r.at)),
        h("td", {}, h("span", { class: "actor" }, h("span", { class: "avatar" }, initial(r.actor)), String(r.actor).replace(/^oidc:/, ""))),
        h("td", {}, h("span", { class: "action-tag" }, r.action)), h("td", { class: "id" }, r.subject),
        h("td", {}, h("div", { class: "history-detail" }, detail)));
    })), h("div", { class: "table-foot" }, h("span", {}, "Newest " + rows.length + " changes"))), h("p", { class: "foot" }, "Every change to organizations, teams, projects and clients is recorded with who made it. Each row carries the hash of the one before it, so an edited or removed row shows at the row where it happened."));
  }
  function showChange(r) {
    openSheet({ title: r.action, subtitle: fmtTime(r.at), body: [
      h("dl", { class: "kv" }, h("dt", {}, "Who"), h("dd", {}, r.actor), h("dt", {}, "Subject"), h("dd", { class: "m" }, r.subject), h("dt", {}, "Entry"), h("dd", {}, "#" + r.id)),
      h("div", { class: "section" }, h("h3", {}, "Detail"), h("pre", { class: "json" }, JSON.stringify(r.detail, null, 2) ?? "—"))] });
  }
  async function verify(e) {
    const button = e.currentTarget;
    button.disabled = true;
    result.replaceChildren(h("div", { class: "notice" }, "Checking the chain…"));
    try {
      const r = await api("/v1/history/verify");
      result.replaceChildren(r.ok
        ? h("div", { class: "notice ok" }, icon("shield", 16), h("div", {}, h("b", {}, "The history is intact."), h("div", { class: "muted" }, fmtInt(r.records) + " rows are chained" + (r.unchained ? "; " + fmtInt(r.unchained) + " older rows from before the chain cannot be vouched for" : "") + ". Keep a copy of the head hash where the database's writer cannot reach, because the chain alone cannot show that the last rows were removed."), h("div", { class: "gap-top copy-row" }, h("span", { class: "mono wrap" }, r.head), copyButton(r.head, "head hash"))))
        : h("div", { class: "notice bad" }, icon("alert", 16), h("div", {}, h("b", {}, "The chain is broken at entry " + r.row + "."), h("div", { class: "muted" }, r.reason))));
    } catch (error) {
      result.replaceChildren();
      fail(error);
    } finally {
      button.disabled = false;
    }
  }
  const size = h("select", { class: "select", "aria-label": "Rows", onchange: (e) => { historyLimit = Number(e.target.value); load().catch(fail); } }, [100, 500, 1000].map((n) => h("option", { value: n, selected: n === historyLimit }, "Newest " + n)));
  ctx.header("History", "Who changed what", [size, canAdmin() ? h("button", { class: "btn", type: "button", onclick: verify }, icon("shield", 14), "Check integrity") : null]);
  page.append(result, body);
  await load();
  return page;
}

// ---- Gateways (read-only) ------------------------------------------------------------------

// Providers, deployments and routes as each gateway reports them. The control plane only reads these from the
// gateways' admin listeners; the configuration stays a file under version control.
async function viewGateways(ctx) {
  const page = h("div", { class: "stack" });
  const fail = showErrorFor(page);
  const scalar = (v) => (v === null || v === undefined ? "—" : typeof v === "object" ? JSON.stringify(v) : String(v));
  const circuit = (c) => h("span", { class: "pill" }, h("i", { class: "dot " + (c === "closed" ? "good" : c === "half_open" ? "warn" : c === "unknown" ? "" : "bad") }), c.replace("_", " "));
  const metrics = (obj) => h("dl", { class: "metrics" }, Object.entries(obj).filter(([, v]) => v !== undefined).map(([k, v]) => h("div", {}, h("dt", {}, k), h("dd", {}, scalar(v)))));

  function panel(g) {
    const stats = g.stats && typeof g.stats === "object" ? g.stats : null, cfg = g.config && typeof g.config === "object" ? g.config : null;
    const lanes = new Map(arr(stats && stats.lanes).map((l) => [l.provider, l]));
    const state = g.error ? ["bad", g.error.replace(/_/g, " ")] : g.ready === true ? ["good", "Ready"] : g.ready === false ? ["bad", "Not ready"] : ["warn", "Unknown"];
    const parts = [h("div", { class: "gw-head" }, h("h2", {}, g.name), h("span", { class: "mono muted" }, g.url), h("span", { class: "pill" }, h("i", { class: "dot " + state[0] }), state[1]),
      g.reachable === false ? h("span", { class: "muted" }, "cannot be reached") : null)];
    if (stats) {
      parts.push(metrics({ Mode: stats.mode, "Caller auth": stats.auth_enforced ? "enforced" : "open" }));
      const circuits = new Map(arr(stats.provider_circuits).map((p) => [p.provider, p.circuit]));
      const names = arr(stats.providers).slice().sort();
      parts.push(h("div", { class: "section" }, h("h3", {}, "Providers"), names.length ? tableOf(["Provider", "Circuit", "Queued", "Dead-lettered"], names.map((name) => h("tr", {},
        h("td", {}, name), h("td", {}, circuits.has(name) ? circuit(circuits.get(name)) : "—"), h("td", { class: "num" }, lanes.has(name) ? fmtInt(lanes.get(name).queue_depth) : "—"),
        h("td", { class: "num" }, lanes.has(name) && lanes.get(name).dead_letter !== undefined ? fmtInt(lanes.get(name).dead_letter) : "—"))), ["l", "l", "r", "r"]) : h("p", { class: "muted" }, "None registered.")));
      if (arr(stats.deployments).length) parts.push(h("div", { class: "section" }, h("h3", {}, "Deployments"), tableOf(["Deployment", "Strategy", "Targets"], arr(stats.deployments).map((d) => h("tr", {},
        h("td", {}, "deployment/" + d.name), h("td", {}, d.strategy), h("td", {}, h("div", { class: "chips" }, arr(d.targets).map((t) => h("span", { class: "chip" }, t.model, h("small", {}, "×" + t.weight), circuit(t.circuit))))))))));
      if (arr(stats.routes).length) parts.push(h("div", { class: "section" }, h("h3", {}, "Named routes"), tableOf(["Route", "Model", "Request defaults"], arr(stats.routes).map((r) => h("tr", {},
        h("td", {}, "route/" + r.name), h("td", {}, r.model), h("td", { class: "wrap" }, Object.keys(r.settings || {}).length ? Object.entries(r.settings).map(([k, v]) => k + " = " + scalar(v)).join(", ") : "—"))))));
      if (stats.outage_admission) parts.push(h("p", { class: "muted" }, "Outage admission: " + (stats.outage_admission.active ? "serving from a lease" : "idle")));
    }
    if (cfg) {
      parts.push(h("div", { class: "section" }, h("h3", {}, "Configuration"), metrics({
        "In sync with its file": cfg.in_sync === undefined ? undefined : cfg.in_sync ? "yes" : "no", Generation: cfg.generation, "Router generation": cfg.router_generation, "Limits generation": cfg.limits_generation,
        "Caller credentials": cfg.credentials, "File hash": typeof cfg.config_sha256 === "string" ? cfg.config_sha256.slice(0, 12) : undefined, Loaded: cfg.loaded_at_epoch_secs ? ago(new Date(cfg.loaded_at_epoch_secs * 1000).toISOString()) : undefined })));
      const clip = (t) => (t.length > 240 ? t.slice(0, 240) + "…" : t);
      if (cfg.last_reload) parts.push(h("p", { class: "muted wrap" }, "Last reload: " + clip(JSON.stringify(cfg.last_reload))));
      if (arr(cfg.env_secret_providers).length) parts.push(h("p", { class: "muted" }, "Provider secrets read from the environment (a reload cannot rotate these): " + cfg.env_secret_providers.join(", ")));
      if (cfg.snapshot) parts.push(h("p", { class: "muted wrap" }, "Policy snapshot: " + clip(JSON.stringify(cfg.snapshot))));
      if (cfg.audit) parts.push(h("p", { class: "muted wrap" }, "Audit log: " + clip(JSON.stringify(cfg.audit))));
    }
    return h("section", { class: "card" }, h("div", { class: "card-body stack" }, parts));
  }
  async function load() {
    const answer = await api("/v1/gateways");
    if (!answer.configured) {
      page.replaceChildren(h("div", { class: "card" }, emptyState({ icon: "gateways", title: "No gateways are listed", text: "Start the control plane with --gateways-file naming each gateway's admin listener to see its providers, circuits, routes and configuration state here." })));
      return;
    }
    // One gateway whose answer is not shaped as expected (an older or newer version) must not hide the others.
    const safe = (g) => { try { return panel(g); } catch (_) { return h("section", { class: "card" }, h("div", { class: "card-body" }, h("b", {}, scalar(g && g.name)), h("p", { class: "muted" }, "This gateway's status could not be displayed."))); } };
    page.replaceChildren(...arr(answer.gateways).map(safe), h("p", { class: "foot" }, "Read from each gateway's admin listener" + (answer.fetched_at ? " as of " + fmtTime(answer.fetched_at) : "") + " (shared for a few seconds). A circuit is that process's own view: “open” means calls to that provider are refused until its cooldown ends. The configuration is a file under version control; this page shows it and does not change it."));
  }
  const refresh = h("button", { class: "btn", type: "button" }, icon("refresh", 14), "Refresh");
  refresh.addEventListener("click", async () => { refresh.disabled = true; try { await load(); } catch (e) { fail(e); } finally { refresh.disabled = false; } });
  ctx.header("Gateways", "Providers, circuits, routes and configuration, as each gateway reports them", refresh);
  await load();
  return page;
}
