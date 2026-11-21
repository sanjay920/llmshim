// Headless-browser smoke test of the admin UI (note 0055). Started by run.sh against a real control plane. It seeds two
// requests, then fails on: a page error, a console error or warning (which includes every CSP violation), a request to
// any origin but the control plane's own, a section that renders nothing, and a secret that is not shown exactly once.
import puppeteer from "puppeteer-core";

const { BASE, TOKEN, INGEST, CHROME } = process.env;
const fail = (message) => { console.error("FAIL: " + message); process.exitCode = 1; };
const wait = (ms) => new Promise((r) => setTimeout(r, ms));
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
page.on("console", (m) => { if (["error", "warning"].includes(m.type())) problems.push("console: " + m.text()); });
page.on("pageerror", (e) => problems.push("pageerror: " + e.message));
page.on("dialog", async (d) => { problems.push("dialog: " + d.message()); await d.dismiss(); });
page.on("request", (r) => {
  const url = r.url();
  if (!url.startsWith("data:") && !url.startsWith("blob:") && new URL(url).origin !== origin) problems.push("foreign request: " + url);
});
const clickText = (label) => page.$$eval("button", (bs, l) => {
  const b = bs.find((x) => x.textContent.trim() === l);
  if (!b) throw new Error("no button " + l);
  b.click();
}, label);

const shell = await fetch(`${BASE}/ui/`);
const csp = shell.headers.get("content-security-policy") || "";
if (!/default-src 'none'|default-src 'self'/.test(csp) || /script-src[^;]*unsafe/.test(csp)) fail("the UI's CSP is missing or loose: " + csp);

// A wrong token is refused; the right one opens the sections.
await page.goto(`${BASE}/ui/`);
await page.waitForSelector("input[type=password]");
await page.type("input[type=password]", "not-the-token-0123456789-abcdef");
await page.click("form button[type=submit]");
await wait(800);
// The refusal is the browser's own console line for the 401 we asked for; nothing else is excused.
for (let i = problems.length - 1; i >= 0; i--) if (problems[i].includes("status of 401")) problems.splice(i, 1);
if (await page.$(".nav a")) fail("a wrong token signed in");
await page.$eval("input[type=password]", (e) => { e.value = ""; });
await page.type("input[type=password]", TOKEN);
await page.click("form button[type=submit]");
await page.waitForSelector(".nav a", { timeout: 10_000 });

const keys = await page.$$eval(".nav a", (as) => as.map((a) => a.dataset.key));
if (keys.length < 5) fail("only " + keys.length + " sections in the navigation: " + keys);
for (const key of keys) {
  await page.evaluate((k) => { location.hash = "#/" + k; }, key);
  await wait(900);
  const text = await page.$eval("main", (e) => e.innerText.trim()).catch(() => "");
  if (text.length < 20) fail(`the ${key} section rendered ${text.length} characters`);
}

// Requests: the seeded rows, a sheet that opens and closes, and the verdict on the changed prompt.
await page.evaluate(() => { location.hash = "#/requests"; });
await wait(900);
const rows = await page.$$("tbody tr.row");
if (rows.length < 2) fail("the Requests table shows " + rows.length + " rows, expected the 2 seeded");
else {
  await rows[0].click();
  await page.waitForSelector(".sheet", { timeout: 5000 }).catch(() => fail("a request's sheet did not open"));
  await wait(1200); // the structure verdict is fetched after the sheet opens
  const sheet = await page.$eval(".sheet", (e) => e.innerText).catch(() => "");
  if (!/first change|changed|system/i.test(sheet)) fail("the sheet does not say where the prompt changed: " + sheet.slice(0, 200));
  await page.keyboard.press("Escape");
  await wait(300);
  if (await page.$(".sheet")) fail("Escape did not close the sheet");
}

// A client is created and its secret is shown once.
await page.evaluate(() => { location.hash = "#/admin"; });
await wait(700);
await clickText("New organization");
await page.waitForSelector('input[placeholder="Organization name"]');
await page.type('input[placeholder="Organization name"]', "Smoke Org");
await clickText("Create organization"); await wait(800);
await clickText("Team");
await page.waitForSelector('input[aria-label="Team name"]');
await page.type('input[aria-label="Team name"]', "Platform");
await page.keyboard.press("Enter"); await wait(800);
await clickText("Project");
await page.waitForSelector('input[aria-label="Project name"]');
await page.type('input[aria-label="Project name"]', "Checkout");
await page.keyboard.press("Enter"); await wait(800);
await page.evaluate(() => { location.hash = "#/clients"; });
await wait(700);
await clickText("New client");
await page.waitForSelector('input[placeholder="e.g. checkout-prod"]');
await page.type('input[placeholder="e.g. checkout-prod"]', "smoke-client");
await page.$eval('input[placeholder="e.g. checkout-prod"]', (e) => e.form.requestSubmit());
await wait(1000);
const shown = await page.$eval("input.secret", (e) => e.value).catch(() => "");
if (shown.length < 20) fail("no secret was shown for the new client");
await clickText("I have stored it"); await wait(400);
if (await page.$("input.secret")) fail("the secret is still on screen after it was stored");
if (shown && (await page.content()).includes(shown)) fail("the secret is still in the page after it was dismissed");

if (problems.length) fail("the page reported:\n  " + problems.join("\n  "));
await browser.close();
console.log(process.exitCode ? "ui smoke: FAILED" : `ui smoke: ok (${keys.length} sections, ${rows.length} request rows)`);
