// The management views (design note 0051, slice 3): Clients (the caller keys), Admin (organizations,
// teams, projects, team budgets) and History (the change log and its chain check). It loads before
// app.js and uses its helpers (h, api, session, fmt*, showErrorFor) only when a view is opened.
// As everywhere in the UI, values from the API are put on the page as text, never as markup. The
// buttons a role may not use are not shown; the API refuses them in any case.
"use strict";

const canIssue = () => ["admin", "key-issuer"].includes(session.who && session.who.role);
const canAdmin = () => (session.who && session.who.role) === "admin";

const send = (method, path, body) => api(path, null, { method, body });

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
  if (isNaN(d)) throw new Error("expires must be a date and time");
  return d.toISOString().replace(/\.\d{3}Z$/, "Z");
}
// The value a datetime-local input takes for an instant: the person's own clock, to the minute. (`toISOString`
// is UTC, which the input would read as local time and shift the expiry by the offset on every save.)
function localInput(iso) {
  const d = new Date(iso);
  if (isNaN(d)) return "";
  const pad = (n) => String(n).padStart(2, "0");
  return d.getFullYear() + "-" + pad(d.getMonth() + 1) + "-" + pad(d.getDate()) + "T" + pad(d.getHours()) + ":" + pad(d.getMinutes());
}
// Run a form's action with its submit button off, so a double click or a second Enter is one request.
async function once(event, action) {
  event.preventDefault();
  const button = event.submitter;
  if (button) { if (button.disabled) return; button.disabled = true; }
  try { await action(); } finally { if (button) button.disabled = false; }
}
const list = (text) => text.split(",").map((x) => x.trim()).filter(Boolean);

function field(label, input, hint) {
  return h("label", { class: "field" }, h("span", {}, label), input, hint ? h("small", { class: "muted" }, hint) : null);
}
function messageBox() {
  return h("div", { class: "err", role: "alert" });
}
// Run an action, show its failure beside the form, and report whether it worked.
async function attempt(box, action) {
  box.textContent = "";
  try {
    await action();
    return true;
  } catch (error) {
    if (error instanceof ApiError && error.status === 401) { showError(error); return false; }
    box.textContent = error.message || String(error);
    return false;
  }
}
// A button that asks twice before it does something that cannot be undone.
function confirmButton(label, what, action) {
  const button = h("button", { type: "button" }, label);
  let armed = false;
  let timer = null;
  const reset = () => { armed = false; clearTimeout(timer); button.textContent = label; button.classList.remove("danger"); };
  button.addEventListener("click", async () => {
    if (!armed) {
      armed = true;
      button.textContent = "Confirm: " + what;
      button.classList.add("danger");
      timer = setTimeout(reset, 6000);
      return;
    }
    reset();
    button.disabled = true;
    try { await action(); } finally { button.disabled = false; }
  });
  return button;
}

// A secret is shown here once and nowhere else: it is not stored by the server and not kept by the page.
function secretPanel(title, secret, onclose) {
  const input = h("input", { type: "text", readonly: true, value: secret, "aria-label": "Secret", class: "secret" });
  input.addEventListener("focus", () => input.select());
  const copied = h("span", { class: "muted" });
  return h("div", { class: "panel" },
    h("strong", {}, title),
    h("p", {}, "This is the only time the secret is shown. Copy it now; it cannot be shown again."),
    input,
    h("div", { class: "filters" },
      h("button", {
        type: "button", onclick: async () => {
          try { await navigator.clipboard.writeText(secret); copied.textContent = "Copied."; } catch (_) { input.select(); copied.textContent = "Select and copy it by hand."; }
        },
      }, "Copy"),
      h("button", { type: "button", onclick: onclose }, "I have stored it"), copied));
}

// ---- Clients -------------------------------------------------------------------------------

async function viewClients() {
  const orgs = (await api("/v1/orgs")).orgs || [];
  const view = h("div", {}, h("h1", {}, "Clients"));
  if (!orgs.length) {
    view.append(h("p", { class: "muted" }, "There is no organization yet." + (canAdmin() ? " Create one in Admin." : "")));
    return view;
  }
  let org = orgs[0].id;
  let projects = new Map(); // project id -> "team / project"
  let keys = [];
  const body = h("div");
  const detail = h("div");
  const fail = showErrorFor(view);

  // While a one-time secret is on screen the page keeps it: no refresh, organization change or other
  // panel may replace it until the person says they have stored it.
  let holding = false;
  async function load() {
    const mine = org;
    try {
      await fetchAll(mine);
    } catch (error) {
      // An answer for an organization the person has already left is not shown over the one they are on.
      if (mine === org) throw error;
    }
  }
  // After a create or rotate: a failed refresh must say so beside the secret, not replace it.
  async function refresh() {
    try {
      await load();
    } catch (error) {
      if (holding && !(error instanceof ApiError && error.status === 401)) {
        body.append(h("p", { class: "err", role: "alert" }, "The list could not be refreshed: " + error.message));
      } else {
        fail(error);
      }
    }
  }
  async function fetchAll(mine) {
    const [teams, list] = await Promise.all([api("/v1/orgs/" + encodeURIComponent(org) + "/teams"), api("/v1/orgs/" + encodeURIComponent(org) + "/keys")]);
    const perTeam = await Promise.all((teams.teams || []).map(async (t) => [t, (await api("/v1/teams/" + encodeURIComponent(t.id) + "/projects")).projects || []]));
    if (mine !== org) return;
    projects = new Map();
    for (const [t, ps] of perTeam) for (const p of ps) projects.set(p.id, t.name + " / " + p.name);
    keys = list.keys || [];
    render();
  }

  function render() {
    if (!holding) detail.replaceChildren();
    const rows = keys.map((k) => h("tr", { class: "row", tabindex: "0", onclick: () => show(k), onkeydown: (e) => { if (e.key === "Enter") show(k); } },
      h("td", {}, k.name), h("td", {}, projects.get(k.project_id) || k.project_id),
      h("td", {}, h("span", { class: "pill " + (k.state === "active" ? "completed" : "http_failure") }, k.state)),
      h("td", {}, k.kind), h("td", { class: "num" }, k.tier),
      h("td", { class: "num" }, k.rpm === null ? "—" : fmtInt(k.rpm)), h("td", { class: "num" }, k.tpm === null ? "—" : fmtInt(k.tpm)),
      h("td", { class: "num" }, k.budget_usd === null ? "—" : fmtUsd(k.budget_usd)),
      h("td", {}, k.expires_at ? fmtTime(k.expires_at) : "never"), h("td", {}, fmtTime(k.created_at))));
    body.replaceChildren(...[
      h("div", { class: "scroll" }, h("table", {},
        h("thead", {}, h("tr", {}, ["Name", "Project", "State", "Credential"].map((x) => h("th", {}, x)),
          ["Tier", "RPM", "TPM", "Budget"].map((x) => h("th", { class: "num" }, x)), h("th", {}, "Expires"), h("th", {}, "Created"))),
        h("tbody", {}, rows))),
      keys.length ? null : h("p", { class: "muted" }, "No clients in this organization yet."),
      h("p", { class: "foot" }, "A client is a caller key. Revoking takes effect when the gateways take their next snapshot. A budget is per window; “—” means no limit.")].filter(Boolean));
  }

  function show(k) {
    if (holding) return;
    const box = messageBox();
    const fields = [
      ["Id", k.id], ["Project", projects.get(k.project_id) || k.project_id], ["State", k.state + (k.revoked_at ? " since " + fmtTime(k.revoked_at) : "")],
      ["Credential", k.kind + (k.client_san ? " " + k.client_san : "") + (k.jwt_table ? " " + k.jwt_table + " / " + k.jwt_subject : "")],
      ["Expires", k.expires_at || "never"], ["Tier", k.tier], ["RPM / TPM", (k.rpm ?? "—") + " / " + (k.tpm ?? "—")],
      ["Budget", k.budget_usd === null ? "none" : fmtUsd(k.budget_usd) + (k.budget_window_secs ? " per " + k.budget_window_secs + " s" : "") + (k.budget_allow_unpriced ? ", unpriced allowed" : "")],
      ["Allowed models", k.allowed_models ? k.allowed_models.join(", ") : "any"], ["Allowed addresses", k.allowed_cidrs ? k.allowed_cidrs.join(", ") : "any"],
      ["Tags", Object.keys(k.tags || {}).length ? Object.entries(k.tags).map(([a, b]) => a + "=" + b).join(", ") : "—"], ["Created", k.created_at],
    ];
    const panel = h("div", { class: "panel" },
      h("div", { class: "filters" }, h("strong", {}, "Client " + k.name), h("button", { type: "button", onclick: () => detail.replaceChildren() }, "Close")),
      h("dl", {}, fields.flatMap(([a, b]) => [h("dt", {}, a), h("dd", {}, b)])));
    if (canIssue() && k.state === "active") {
      panel.append(limitsForm(k, box), rotateForm(k, box),
        h("div", { class: "filters" }, confirmButton("Revoke", "revoke " + k.name, async () => {
          if (await attempt(box, async () => { await send("POST", "/v1/keys/" + encodeURIComponent(k.id) + "/revoke"); })) await load().catch(fail);
        })), box);
    } else if (!canIssue()) {
      panel.append(h("p", { class: "muted" }, "Your role can read clients but not change them."));
    }
    detail.replaceChildren(panel);
    detail.scrollIntoView({ block: "nearest" });
  }

  function limitsForm(k, box) {
    const rpm = h("input", { type: "text", inputmode: "numeric", value: k.rpm ?? "" });
    const tpm = h("input", { type: "text", inputmode: "numeric", value: k.tpm ?? "" });
    const budget = h("input", { type: "text", inputmode: "decimal", value: k.budget_usd ?? "" });
    const window = h("input", { type: "text", inputmode: "numeric", value: k.budget_window_secs ?? "" });
    const until = h("input", { type: "datetime-local", value: k.expires_at ? localInput(k.expires_at) : "" });
    return h("form", {
      class: "formgrid", onsubmit: (e) => once(e, async () => {
        if (await attempt(box, async () => {
          await send("PATCH", "/v1/keys/" + encodeURIComponent(k.id) + "/limits", {
            rpm: wholeNumber(rpm.value, "RPM", 1), tpm: wholeNumber(tpm.value, "TPM", 1), budget_usd: decimal(budget.value, "Budget"),
            budget_window_secs: wholeNumber(window.value, "Budget window", 1), expires_at: expiry(until.value),
          });
        })) await load().catch(fail);
      }),
    }, h("strong", {}, "Limits"), h("span", { class: "muted" }, "Blank clears a limit."),
      field("RPM", rpm), field("TPM", tpm), field("Budget, USD", budget), field("Budget window, seconds", window), field("Expires (your time)", until),
      h("button", { type: "submit" }, "Save limits"));
  }

  function rotateForm(k, box) {
    const name = h("input", { type: "text", value: k.name + "-next", maxlength: "128" });
    return h("form", {
      class: "formgrid", onsubmit: (e) => once(e, async () => {
        let made = null;
        if (await attempt(box, async () => { made = await send("POST", "/v1/keys/" + encodeURIComponent(k.id) + "/rotate", { name: name.value.trim() }); })) {
          // The secret first: nothing that can fail after the key exists may stand between it and the person.
          showSecret("New client " + made.key.name, made.secret);
          await refresh();
        }
      }),
    }, h("strong", {}, "Rotate"), h("span", { class: "muted" }, "Issues a replacement with the same limits; this one stays active until you revoke it."),
      field("Name of the replacement", name), h("button", { type: "submit" }, "Rotate"));
  }

  function showSecret(title, secret) {
    holding = !!secret;
    orgSelect.disabled = holding;
    detail.replaceChildren(secret ? secretPanel(title, secret, () => { holding = false; orgSelect.disabled = false; detail.replaceChildren(); }) : h("div", { class: "panel" }, h("strong", {}, title + " created")));
  }

  function createForm() {
    const box = messageBox();
    const project = h("select", { "aria-label": "Project" }, [...projects].map(([id, label]) => h("option", { value: id }, label)));
    const name = h("input", { type: "text", maxlength: "128", required: true });
    const kind = h("select", { "aria-label": "Credential" }, ["secret", "client_san", "jwt"].map((x) => h("option", { value: x }, x)));
    const san = h("input", { type: "text", placeholder: "dns:name or uri:…" });
    const table = h("input", { type: "text", placeholder: "issuer table" });
    const subject = h("input", { type: "text", placeholder: "exact sub" });
    const tier = h("input", { type: "text", inputmode: "numeric", value: "0" });
    const rpm = h("input", { type: "text", inputmode: "numeric" });
    const tpm = h("input", { type: "text", inputmode: "numeric" });
    const budget = h("input", { type: "text", inputmode: "decimal" });
    const window = h("input", { type: "text", inputmode: "numeric" });
    const models = h("input", { type: "text", placeholder: "comma-separated; blank allows any" });
    const cidrs = h("input", { type: "text", placeholder: "comma-separated; blank allows any" });
    const until = h("input", { type: "datetime-local" });
    const extra = h("div", { class: "formgrid" });
    const syncKind = () => extra.replaceChildren(...(kind.value === "client_san" ? [field("Client certificate identity", san)] : kind.value === "jwt" ? [field("Issuer table", table), field("Subject", subject)] : []));
    kind.addEventListener("change", syncKind);
    syncKind();
    return h("form", {
      class: "panel formgrid", onsubmit: (e) => once(e, async () => {
        let made = null;
        if (await attempt(box, async () => {
          const body = {
            name: name.value.trim(), tier: wholeNumber(tier.value, "Tier", 0) ?? 0, rpm: wholeNumber(rpm.value, "RPM", 1), tpm: wholeNumber(tpm.value, "TPM", 1),
            budget_usd: decimal(budget.value, "Budget"), budget_window_secs: wholeNumber(window.value, "Budget window", 1),
            allowed_models: list(models.value).length ? list(models.value) : null, allowed_cidrs: list(cidrs.value).length ? list(cidrs.value) : null,
            expires_at: expiry(until.value),
            credential: kind.value === "client_san" ? { type: "client_san", san: san.value.trim() } : kind.value === "jwt" ? { type: "jwt", table: table.value.trim(), subject: subject.value.trim() } : { type: "secret" },
          };
          made = await send("POST", "/v1/projects/" + encodeURIComponent(project.value) + "/keys", body);
        })) {
          showSecret("Client " + made.key.name, made.secret);
          await refresh();
        }
      }),
    }, h("strong", {}, "New client"), field("Project", project), field("Name", name), field("Credential", kind), extra,
      field("Tier", tier, "Higher tiers are queued first."), field("RPM", rpm), field("TPM", tpm), field("Budget, USD", budget), field("Budget window, seconds", window),
      field("Allowed models", models), field("Allowed addresses (CIDR)", cidrs), field("Expires (your time)", until),
      h("button", { type: "submit", class: "primary" }, "Create client"), box);
  }

  const orgSelect = h("select", { "aria-label": "Organization", onchange: (e) => { org = e.target.value; load().catch(fail); } },
    orgs.map((o) => h("option", { value: o.id }, o.name)));
  const create = h("div");
  view.append(h("div", { class: "filters" }, orgSelect,
    canIssue() ? h("button", { type: "button", class: "primary", onclick: () => {
      if (holding) return;
      if (!projects.size) { detail.replaceChildren(h("p", { class: "muted" }, "This organization has no project yet. Create a team and a project in Admin first.")); return; }
      detail.replaceChildren(createForm());
    } }, "New client") : null), body, create, detail);
  await load();
  return view;
}

// ---- Admin ---------------------------------------------------------------------------------

async function viewAdmin() {
  const view = h("div", {}, h("h1", {}, "Organizations, teams and projects"));
  const fail = showErrorFor(view);
  const body = h("div");
  view.append(body);

  async function load() {
    const orgs = (await api("/v1/orgs")).orgs || [];
    const trees = await Promise.all(orgs.map(async (o) => {
      const teams = (await api("/v1/orgs/" + encodeURIComponent(o.id) + "/teams")).teams || [];
      const withProjects = await Promise.all(teams.map(async (t) => [t, (await api("/v1/teams/" + encodeURIComponent(t.id) + "/projects")).projects || []]));
      return [o, withProjects];
    }));
    body.replaceChildren(...[
      canAdmin() ? addForm("New organization", "Name", async (name) => { await send("POST", "/v1/orgs", { name }); }) : h("p", { class: "muted" }, "Your role can read this but not change it."),
      trees.length ? null : h("p", { class: "muted" }, "Nothing here yet."),
      ...trees.map(([o, teams]) => h("div", { class: "panel" },
        h("strong", {}, o.name), h("span", { class: "sub" }, "  " + o.id + " · created " + fmtTime(o.created_at)),
        canAdmin() ? addForm("New team", "Team name", async (name) => { await send("POST", "/v1/orgs/" + encodeURIComponent(o.id) + "/teams", { name }); }) : null,
        teams.length ? null : h("p", { class: "muted" }, "No teams."),
        ...teams.map(([t, projects]) => h("div", { class: "sub-panel" },
          h("strong", {}, t.name), h("span", { class: "sub" }, "  " + t.id),
          h("div", { class: "muted" }, t.budget ? "Budget " + fmtUsd(t.budget.budget_usd) + (t.budget.budget_window_secs ? " per " + t.budget.budget_window_secs + " s" : "") + (t.budget.budget_allow_unpriced ? ", unpriced allowed" : "") : "No team budget"),
          canAdmin() ? budgetForm(t) : null,
          h("ul", {}, projects.map((p) => h("li", {}, p.name, h("span", { class: "sub" }, "  " + p.id)))),
          canAdmin() ? addForm("New project", "Project name", async (name) => { await send("POST", "/v1/teams/" + encodeURIComponent(t.id) + "/projects", { name }); }) : null))))].filter(Boolean));
  }

  function addForm(title, label, action) {
    const box = messageBox();
    const name = h("input", { type: "text", maxlength: "128", required: true, placeholder: label, "aria-label": label });
    return h("form", {
      class: "filters", onsubmit: (e) => once(e, async () => {
        if (await attempt(box, async () => { await action(name.value.trim()); })) await load().catch(fail);
      }),
    }, name, h("button", { type: "submit" }, title), box);
  }

  function budgetForm(t) {
    const box = messageBox();
    const usd = h("input", { type: "text", inputmode: "decimal", value: t.budget ? t.budget.budget_usd : "", placeholder: "USD", "aria-label": "Budget, USD" });
    const window = h("input", { type: "text", inputmode: "numeric", value: t.budget && t.budget.budget_window_secs ? t.budget.budget_window_secs : "", placeholder: "window, s", "aria-label": "Window, seconds" });
    const unpriced = h("input", { type: "checkbox", checked: !!(t.budget && t.budget.budget_allow_unpriced) });
    return h("form", {
      class: "filters", onsubmit: (e) => once(e, async () => {
        if (await attempt(box, async () => {
          const amount = decimal(usd.value, "Budget");
          if (amount === null) throw new Error("Enter a budget, or use Clear budget");
          await send("PUT", "/v1/teams/" + encodeURIComponent(t.id) + "/budget", {
            budget_usd: amount, budget_window_secs: wholeNumber(window.value, "Window", 1), budget_allow_unpriced: unpriced.checked,
          });
        })) await load().catch(fail);
      }),
    }, usd, window, h("label", { class: "muted" }, unpriced, " allow unpriced"), h("button", { type: "submit" }, "Set budget"),
      t.budget ? confirmButton("Clear budget", "clear the budget of " + t.name, async () => {
        if (await attempt(box, async () => { await send("DELETE", "/v1/teams/" + encodeURIComponent(t.id) + "/budget"); })) await load().catch(fail);
      }) : null, box);
  }

  await load();
  return view;
}

// ---- History -------------------------------------------------------------------------------

async function viewHistory() {
  let limit = 100;
  const tbody = h("tbody");
  const result = h("div");
  const view = h("div", {}, h("h1", {}, "Change history"));
  const fail = showErrorFor(view);

  async function load() {
    const rows = (await api("/v1/history", { limit })).history || [];
    tbody.replaceChildren(...rows.map((r) => {
      const detail = r.detail === null || r.detail === undefined ? "" : JSON.stringify(r.detail);
      return h("tr", {}, h("td", { class: "num" }, r.id), h("td", {}, fmtTime(r.at)), h("td", {}, r.actor), h("td", {}, r.action), h("td", {}, r.subject),
        h("td", { class: "wrap", title: detail }, detail.length > 140 ? detail.slice(0, 140) + "…" : detail));
    }));
    if (!rows.length) tbody.replaceChildren(h("tr", {}, h("td", { colspan: "6", class: "muted" }, "No changes recorded.")));
  }

  const verify = h("button", { type: "button" }, "Check the chain");
  verify.hidden = !canAdmin();
  verify.addEventListener("click", async () => {
    verify.disabled = true;
    result.replaceChildren(h("p", { class: "muted" }, "Checking…"));
    try {
      const r = await api("/v1/history/verify");
      result.replaceChildren(r.ok
        ? h("div", { class: "panel" }, h("strong", { class: "okc" }, "The chain checks out."),
          h("p", {}, fmtInt(r.records) + " rows are chained" + (r.unchained ? "; " + fmtInt(r.unchained) + " older rows from before the chain cannot be vouched for" : "") + "."),
          h("p", { class: "muted" }, "Head hash — keep a copy somewhere the database's writer cannot reach; the chain alone cannot show that the last rows were removed:"),
          h("code", { class: "wrap" }, r.head))
        : h("div", { class: "panel" }, h("strong", { class: "err" }, "The chain is broken at row " + r.row + "."), h("p", {}, r.reason)));
    } catch (error) {
      fail(error);
    } finally {
      verify.disabled = false;
    }
  });
  const size = h("select", { "aria-label": "Rows", onchange: (e) => { limit = Number(e.target.value); load().catch(fail); } },
    [100, 500, 1000].map((n) => h("option", { value: n }, "Newest " + n)));
  view.append(h("div", { class: "filters" }, size, verify, canAdmin() ? null : h("span", { class: "muted" }, "Checking the chain needs the admin role (or `llmshim-control audit-verify` on the server).")), result,
    h("div", { class: "scroll" }, h("table", {}, h("thead", {}, h("tr", {}, h("th", { class: "num" }, "#"), h("th", {}, "Time"), h("th", {}, "Actor"), h("th", {}, "Action"), h("th", {}, "Subject"), h("th", {}, "Detail"))), tbody)),
    h("p", { class: "foot" }, "Every change to organizations, teams, projects and keys is recorded with who made it. Each row carries the hash of the one before it, so an edited or removed row shows at the row where it happened."));
  await load();
  return view;
}

// ---- Gateways (read-only) ------------------------------------------------------------------

// Providers, deployments and routes as each gateway reports them. The control plane only reads these
// from the gateways' admin listeners; the configuration stays a file under version control.
async function viewGateways() {
  const view = h("div", {}, h("h1", {}, "Gateways"));
  const fail = showErrorFor(view);
  const body = h("div");
  const refresh = h("button", { type: "button" }, "Refresh");
  view.append(h("div", { class: "filters" }, refresh), body);

  const circuitPill = (circuit) => h("span", { class: "pill " + (circuit === "closed" ? "completed" : circuit === "half_open" ? "abandoned" : "http_failure") }, circuit);
  const scalar = (value) => (value === null || value === undefined ? "—" : typeof value === "object" ? JSON.stringify(value) : String(value));
  const table = (heads, rows) => h("div", { class: "scroll" }, h("table", {}, h("thead", {}, h("tr", {}, heads.map((x) => h("th", {}, x)))), h("tbody", {}, rows)));
  const kv = (object) => h("dl", {}, Object.entries(object).flatMap(([k, v]) => [h("dt", {}, k), h("dd", {}, scalar(v))]));
  const clip = (text) => (text.length > 200 ? text.slice(0, 200) + "…" : text);

  function gatewayPanel(g) {
    const stats = g.stats || null;
    const cfg = g.config || null;
    const lanes = new Map(((stats && stats.lanes) || []).map((l) => [l.provider, l]));
    const state = g.error
      ? h("span", { class: "pill http_failure" }, g.error.replace(/_/g, " "))
      : g.ready === true ? h("span", { class: "pill completed" }, "ready") : g.ready === false ? h("span", { class: "pill http_failure" }, "not ready") : h("span", { class: "pill abandoned" }, "unknown");
    const parts = [h("div", { class: "filters" }, h("strong", {}, g.name), h("span", { class: "sub" }, g.url), state, g.reachable === false ? h("span", { class: "muted" }, "cannot be reached") : null)];
    if (stats) {
      parts.push(kv({ mode: stats.mode, "caller auth enforced": stats.auth_enforced }));
      const circuits = new Map((stats.provider_circuits || []).map((p) => [p.provider, p.circuit]));
      const names = (stats.providers || []).slice().sort();
      parts.push(h("h2", {}, "Providers"), names.length ? table(["Provider", "Circuit", "Queued", "Dead-lettered"], names.map((name) => h("tr", {},
        h("td", {}, name), h("td", {}, circuits.has(name) ? circuitPill(circuits.get(name)) : "—"),
        h("td", { class: "num" }, lanes.has(name) ? fmtInt(lanes.get(name).queue_depth) : "—"),
        h("td", { class: "num" }, lanes.has(name) && lanes.get(name).dead_letter !== undefined ? fmtInt(lanes.get(name).dead_letter) : "—")))) : h("p", { class: "muted" }, "None registered."));
      if ((stats.deployments || []).length) {
        parts.push(h("h2", {}, "Deployments"), table(["Deployment", "Strategy", "Targets"], stats.deployments.map((d) => h("tr", {},
          h("td", {}, d.name), h("td", {}, d.strategy),
          h("td", {}, d.targets.map((t) => h("div", {}, t.model + " ×" + t.weight + " ", circuitPill(t.circuit))))))));
      }
      if ((stats.routes || []).length) {
        parts.push(h("h2", {}, "Named routes"), table(["Route", "Model", "Request defaults"], stats.routes.map((r) => h("tr", {},
          h("td", {}, "route/" + r.name), h("td", {}, r.model),
          h("td", { class: "wrap" }, Object.keys(r.settings || {}).length ? Object.entries(r.settings).map(([k, v]) => k + "=" + scalar(v)).join(", ") : "—")))));
      }
      if (stats.outage_admission) parts.push(h("p", { class: "muted" }, "Outage admission: " + (stats.outage_admission.active ? "serving from a lease" : "idle")));
    }
    if (cfg) {
      const sync = { "in sync with its file": cfg.in_sync, "configuration generation": cfg.generation, "router generation": cfg.router_generation, "limits generation": cfg.limits_generation,
        "caller credentials": cfg.credentials, "file sha-256": typeof cfg.config_sha256 === "string" ? cfg.config_sha256.slice(0, 16) + "…" : null,
        "loaded at": cfg.loaded_at_epoch_secs ? fmtTime(new Date(cfg.loaded_at_epoch_secs * 1000).toISOString()) : null };
      parts.push(h("h2", {}, "Configuration"), kv(sync));
      if (cfg.last_reload) parts.push(h("p", { class: "muted wrap" }, "Last reload: " + clip(JSON.stringify(cfg.last_reload))));
      if ((cfg.env_secret_providers || []).length) parts.push(h("p", { class: "muted" }, "Provider secrets from the environment (a reload cannot rotate these): " + cfg.env_secret_providers.join(", ")));
      if (cfg.snapshot) parts.push(h("p", { class: "muted wrap" }, "Policy snapshot: " + clip(JSON.stringify(cfg.snapshot))));
      if (cfg.audit) parts.push(h("p", { class: "muted wrap" }, "Audit log: " + clip(JSON.stringify(cfg.audit))));
    }
    return h("div", { class: "panel" }, parts);
  }

  async function load() {
    const answer = await api("/v1/gateways");
    if (!answer.configured) {
      body.replaceChildren(h("p", { class: "muted" }, "No gateways are listed. Start the control plane with --gateways-file naming each gateway's admin listener to see their providers, circuits, routes and configuration state here."));
      return;
    }
    body.replaceChildren(...answer.gateways.map(gatewayPanel),
      h("p", { class: "foot" }, "Read from each gateway's admin listener " + (answer.fetched_at ? "as of " + fmtTime(answer.fetched_at) : "") + " (shared for a few seconds). A circuit is that process's own view: “open” means calls to that provider are refused until its cooldown ends. The configuration is a file under version control; this page shows it and does not change it."));
  }
  refresh.addEventListener("click", async () => {
    refresh.disabled = true;
    try { await load(); } catch (error) { fail(error); } finally { refresh.disabled = false; }
  });
  await load();
  return view;
}
