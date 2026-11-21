// Headless-browser smoke test of the admin UI (note 0055). Started by run.sh against a real control plane. It seeds two
// requests, then fails on: a page error, a console error or warning (which includes every CSP violation), a request to
// any origin but the control plane's own, a section that renders nothing, and a secret that is not shown exactly once.
// Waits are on conditions, not on time; a thrown error still prints everything the page reported before it.
import puppeteer from "puppeteer-core";

const { BASE, TOKEN, INGEST, CHROME } = process.env;
const fail = (message) => { console.error("FAIL: " + message); process.exitCode = 1; };
const origin = new URL(BASE).origin;

// Two requests of one client whose system prompt changed, so the cache debugger has something to say.
const fingerprint = (tag) => Buffer.from(tag).toString("hex").padEnd(16, "0").slice(0, 16);
const seg = (kind, tag) => ({ kind, hash: fingerprint(tag), bytes: 100 });
const record = (id, ts, structure) => ({
  event: "usage", ts, attempt_id: id, tenant: "payments", key: "checkout-prod", request_id: `req-${id}`,
  provider: "openai", model: "gpt-6-sol", kind: "completion", outcome: "completed", duration_ms: 900,
  input_tokens: 1000, output_tokens: 20, uncached_input_tokens: 100, cache_read_tokens: 900, cache_write_tokens: 0,
  cost_usd: 0.01, cost_source: "provider", usage_reported: true, admission: "fleet", structure,
});
const now = Date.now();
const iso = (ms) => new Date(ms).toISOString().replace(/\.\d+Z$/, "Z");
const seeded = await fetch(`${BASE}/v1/usage/batch`, {
  method: "POST",
  headers: { authorization: `Bearer ${INGEST}`, "content-type": "application/json" },
  body: JSON.stringify({ records: [
    record("smoke-1", iso(now - 120_000), [seg("tool", "t1"), seg("system", "v1"), seg("user", "hello")]),
    record("smoke-2", iso(now - 60_000), [seg("tool", "t1"), seg("system", "v2"), seg("user", "hello")]),
  ] }),
});
if (!seeded.ok) { console.error("seeding the ledger: " + seeded.status + " " + (await seeded.text())); process.exit(1); }

const browser = await puppeteer.launch({ executablePath: CHROME, headless: true, args: ["--no-sandbox"] });
const page = await browser.newPage();
await page.setViewport({ width: 1400, height: 1000 });
const problems = [];
const refusals = []; // every 4xx/5xx the page saw, so the 401 this test provokes is matched, not assumed
page.on("console", (m) => { if (["error", "warning"].includes(m.type())) problems.push("console: " + m.text() + " " + (m.location().url || "")); });
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
page.on("dialog", async (d) => { problems.push("dialog: " + d.message()); await d.dismiss(); });
page.on("request", (r) => {
  const url = r.url();
  if (!url.startsWith("data:") && !url.startsWith("blob:") && new URL(url).origin !== origin) problems.push("foreign request: " + url);
});
page.on("response", (r) => { if (r.status() >= 400) refusals.push(`${r.status()} ${new URL(r.url()).pathname}`); });

const clickText = (label) => page.$$eval("button", (bs, l) => {
  const b = bs.find((x) => x.textContent.trim() === l);
  if (!b) throw new Error("no button " + l);
  b.click();
}, label);
const until = (what, fn, arg, timeout = 15_000) =>
  page.waitForFunction(fn, { timeout }, arg).catch(() => { throw new Error("timed out waiting for " + what); });
const visibleText = (selector) => page.$eval(selector, (e) => e.innerText.trim()).catch(() => "");

try {
  const shell = await fetch(`${BASE}/ui/`);
  const csp = shell.headers.get("content-security-policy") || "";
  const directives = Object.fromEntries(csp.split(";").map((d) => d.trim()).filter(Boolean).map((d) => [d.split(" ")[0], d]));
  if (!directives["default-src"]) fail("the UI's CSP has no default-src: " + csp);
  for (const name of ["default-src", "script-src", "style-src", "connect-src"]) {
    if (directives[name] && /'unsafe-|\*/.test(directives[name])) fail(`the UI's CSP ${name} is loose: ${directives[name]}`);
  }

  // A wrong token is refused by the server and does not sign in.
  await page.goto(`${BASE}/ui/`);
  await page.waitForSelector("input[type=password]");
  await page.type("input[type=password]", "not-the-token-0123456789-abcdef");
  await page.click("form button[type=submit]");
  for (let i = 0; i < 100 && !refusals.some((r) => r.startsWith("401")); i++) await new Promise((r) => setTimeout(r, 100));
  if (!refusals.some((r) => r.startsWith("401"))) fail("the server never answered the wrong token with a 401: " + JSON.stringify(refusals));
  if (await page.$(".nav a")) fail("a wrong token signed in");
  // The browser's own console line for that provoked 401 is excused, and only that.
  for (let i = problems.length - 1; i >= 0; i--) if (/status of 401/.test(problems[i])) problems.splice(i, 1);

  await page.$eval("input[type=password]", (e) => { e.value = ""; });
  await page.type("input[type=password]", TOKEN);
  await page.click("form button[type=submit]");
  await page.waitForSelector(".nav a", { timeout: 10_000 });

  // Every section becomes current, renders, and shows no error banner.
  const keys = await page.$$eval(".nav a", (as) => as.map((a) => a.dataset.key));
  if (keys.length < 5) fail("only " + keys.length + " sections in the navigation: " + keys);
  for (const key of keys) {
    await page.evaluate((k) => { location.hash = "#/" + k; }, key);
    await until(`the ${key} section to be current`, (k) => document.querySelector(`.nav a[data-key="${k}"]`)?.getAttribute("aria-current") === "page", key);
    await until(`the ${key} section to render`, () => (document.querySelector("main")?.innerText.trim().length || 0) > 20);
    const failed = await page.$$eval(".notice.bad, .error, [role=alert]", (e) => e.map((x) => x.innerText.trim()).filter(Boolean)).catch(() => []);
    if (failed.length) fail(`the ${key} section shows an error: ${failed[0].slice(0, 160)}`);
  }

  // Requests: the seeded rows; the newest (the changed prompt) opens a sheet that names the first change.
  await page.evaluate(() => { location.hash = "#/requests"; });
  await until("the two seeded rows", () => document.querySelectorAll("tbody tr.row").length >= 2);
  const rows = await page.$$("tbody tr.row");
  await rows[0].click(); // newest first: smoke-2, whose system prompt differs from smoke-1's
  await page.waitForSelector(".sheet", { timeout: 5000 });
  await until("the cache verdict", () => /Matched the previous request/.test(document.querySelector(".sheet")?.innerText || ""));
  const sheet = await visibleText(".sheet");
  if (!/first change was at the [^.]*system/i.test(sheet)) fail("the sheet's verdict does not name the system prompt: " + sheet.slice(0, 300));
  await page.keyboard.press("Escape");
  await until("the sheet to close", () => !document.querySelector(".sheet"), null, 5000).catch(() => fail("Escape did not close the sheet"));

  // An organization, team, project and client are created through the forms; the secret is shown once.
  await page.evaluate(() => { location.hash = "#/admin"; });
  await clickText("New organization");
  await page.waitForSelector('input[placeholder="Organization name"]');
  await page.type('input[placeholder="Organization name"]', "Smoke Org");
  await clickText("Create organization");
  await until("the organization to appear", () => /Smoke Org/.test(document.querySelector("main")?.innerText || ""));
  await clickText("Team");
  await page.waitForSelector('input[aria-label="Team name"]');
  await page.type('input[aria-label="Team name"]', "Platform");
  await page.keyboard.press("Enter");
  await until("the team to appear", () => /Platform/.test(document.querySelector("main")?.innerText || ""));
  await clickText("Project");
  await page.waitForSelector('input[aria-label="Project name"]');
  await page.type('input[aria-label="Project name"]', "Checkout");
  await page.keyboard.press("Enter");
  await until("the project to appear", () => /Checkout/.test(document.querySelector("main")?.innerText || ""));
  await page.evaluate(() => { location.hash = "#/clients"; });
  await until("the clients view", () => document.querySelector(".nav a[data-key=clients]")?.getAttribute("aria-current") === "page");
  await clickText("New client");
  await page.waitForSelector('input[placeholder="e.g. checkout-prod"]');
  await page.type('input[placeholder="e.g. checkout-prod"]', "smoke-client");
  await page.$eval('input[placeholder="e.g. checkout-prod"]', (e) => e.form.requestSubmit());
  await page.waitForSelector("input.secret", { timeout: 10_000 });
  const shown = await page.$eval("input.secret", (e) => e.value);
  if (shown.length < 20) fail("the secret shown for the new client is too short to be one");
  await clickText("I have stored it");
  await until("the secret to be dismissed", () => !document.querySelector("input.secret"), null, 5000).catch(() => fail("the secret is still on screen after it was stored"));
  const leaked = await page.evaluate((secret) => document.documentElement.outerHTML.includes(secret) ||
    [...document.querySelectorAll("input,textarea")].some((e) => e.value.includes(secret)), shown);
  if (leaked) fail("the secret is still in the page after it was dismissed");

  console.log(`ui smoke: ${keys.length} sections, ${rows.length} request rows`);
} catch (error) {
  fail(String(error && error.stack ? error.stack : error));
} finally {
  // Whatever the page reported is printed even when the run died on a timeout: it is usually the cause.
  if (problems.length) fail("the page reported:\n  " + problems.join("\n  "));
  await browser.close();
  console.log(process.exitCode ? "ui smoke: FAILED" : "ui smoke: ok");
}
