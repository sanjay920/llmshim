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
  button.addEventListener("click", () => {
    if (!armed) {
      armed = true;
      button.textContent = "Confirm: " + what;
      button.classList.add("danger");
      setTimeout(() => { armed = false; button.textContent = label; button.classList.remove("danger"); }, 6000);
      return;
    }
    action();
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

  async function load() {
    const mine = org;
    const [teams, list] = await Promise.all([api("/v1/orgs/" + encodeURIComponent(org) + "/teams"), api("/v1/orgs/" + encodeURIComponent(org) + "/keys")]);
    const perTeam = await Promise.all((teams.teams || []).map(async (t) => [t, (await api("/v1/teams/" + encodeURIComponent(t.id) + "/projects")).projects || []]));
    if (mine !== org) return;
    projects = new Map();
    for (const [t, ps] of perTeam) for (const p of ps) projects.set(p.id, t.name + " / " + p.name);
    keys = list.keys || [];
    render();
  }

  function render() {
    detail.replaceChildren();
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
    const until = h("input", { type: "datetime-local", value: k.expires_at ? new Date(k.expires_at).toISOString().slice(0, 16) : "" });
    return h("form", {
      class: "formgrid", onsubmit: async (e) => {
        e.preventDefault();
        if (await attempt(box, async () => {
          await send("PATCH", "/v1/keys/" + encodeURIComponent(k.id) + "/limits", {
            rpm: wholeNumber(rpm.value, "RPM", 1), tpm: wholeNumber(tpm.value, "TPM", 1), budget_usd: decimal(budget.value, "Budget"),
            budget_window_secs: wholeNumber(window.value, "Budget window", 1), expires_at: expiry(until.value),
          });
        })) await load().catch(fail);
      },
    }, h("strong", {}, "Limits"), h("span", { class: "muted" }, "Blank clears a limit."),
      field("RPM", rpm), field("TPM", tpm), field("Budget, USD", budget), field("Budget window, seconds", window), field("Expires (your time)", until),
      h("button", { type: "submit" }, "Save limits"));
  }

  function rotateForm(k, box) {
    const name = h("input", { type: "text", value: k.name + "-next", maxlength: "128" });
    return h("form", {
      class: "formgrid", onsubmit: async (e) => {
        e.preventDefault();
        let made = null;
        if (await attempt(box, async () => { made = await send("POST", "/v1/keys/" + encodeURIComponent(k.id) + "/rotate", { name: name.value.trim() }); })) {
          await load().catch(fail);
          showSecret("New client " + made.key.name, made.secret);
        }
      },
    }, h("strong", {}, "Rotate"), h("span", { class: "muted" }, "Issues a replacement with the same limits; this one stays active until you revoke it."),
      field("Name of the replacement", name), h("button", { type: "submit" }, "Rotate"));
  }

  function showSecret(title, secret) {
    detail.replaceChildren(secret ? secretPanel(title, secret, () => detail.replaceChildren()) : h("div", { class: "panel" }, h("strong", {}, title + " created")));
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
      class: "panel formgrid", onsubmit: async (e) => {
        e.preventDefault();
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
          await load().catch(fail);
          showSecret("Client " + made.key.name, made.secret);
        }
      },
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
      class: "filters", onsubmit: async (e) => {
        e.preventDefault();
        if (await attempt(box, async () => { await action(name.value.trim()); })) await load().catch(fail);
      },
    }, name, h("button", { type: "submit" }, title), box);
  }

  function budgetForm(t) {
    const box = messageBox();
    const usd = h("input", { type: "text", inputmode: "decimal", value: t.budget ? t.budget.budget_usd : "", placeholder: "USD", "aria-label": "Budget, USD" });
    const window = h("input", { type: "text", inputmode: "numeric", value: t.budget && t.budget.budget_window_secs ? t.budget.budget_window_secs : "", placeholder: "window, s", "aria-label": "Window, seconds" });
    const unpriced = h("input", { type: "checkbox", checked: !!(t.budget && t.budget.budget_allow_unpriced) });
    return h("form", {
      class: "filters", onsubmit: async (e) => {
        e.preventDefault();
        if (await attempt(box, async () => {
          const amount = decimal(usd.value, "Budget");
          if (amount === null) throw new Error("Enter a budget, or use Clear budget");
          await send("PUT", "/v1/teams/" + encodeURIComponent(t.id) + "/budget", {
            budget_usd: amount, budget_window_secs: wholeNumber(window.value, "Window", 1), budget_allow_unpriced: unpriced.checked,
          });
        })) await load().catch(fail);
      },
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
  view.append(h("div", { class: "filters" }, size, verify), result,
    h("div", { class: "scroll" }, h("table", {}, h("thead", {}, h("tr", {}, h("th", { class: "num" }, "#"), h("th", {}, "Time"), h("th", {}, "Actor"), h("th", {}, "Action"), h("th", {}, "Subject"), h("th", {}, "Detail"))), tbody)),
    h("p", { class: "foot" }, "Every change to organizations, teams, projects and keys is recorded with who made it. Each row carries the hash of the one before it, so an edited or removed row shows at the row where it happened."));
  await load();
  return view;
}
