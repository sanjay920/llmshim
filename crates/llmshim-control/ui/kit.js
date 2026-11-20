// The admin UI's kit (design note 0051): DOM helper, icons, formatting, toasts, slide-over sheets, and the
// charts. No framework and nothing from other origins. Everything that comes from the API is put on the page
// as text (`textContent`, text nodes), never as markup, because names are typed by callers.
"use strict";

// ---- DOM -----------------------------------------------------------------------------------

function h(tag, attrs, ...children) {
  const el = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v === null || v === undefined) continue;
    if (k === "class") el.className = v;
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, v === true ? "" : String(v));
  }
  append(el, children);
  return el;
}
function append(el, children) {
  for (const c of children.flat(Infinity)) {
    if (c === null || c === undefined || c === false) continue;
    el.append(c instanceof Node ? c : document.createTextNode(String(c)));
  }
  return el;
}
const SVG_NS = "http://www.w3.org/2000/svg";
function svg(tag, attrs, ...children) {
  const el = document.createElementNS(SVG_NS, tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === false || v === null || v === undefined) continue;
    if (k === "class") el.setAttribute("class", v);
    else if (k.startsWith("on")) el.addEventListener(k.slice(2), v);
    else el.setAttribute(k, String(v));
  }
  for (const c of children.flat(Infinity)) if (c) el.append(c);
  return el;
}
const $ = (id) => document.getElementById(id);

// ---- icons: one set, 24px grid, 1.6 stroke -------------------------------------------------

const ICONS = {
  home: "M4 11.2 12 4.5l8 6.7V19a1 1 0 0 1-1 1h-4.2v-5.2H9.2V20H5a1 1 0 0 1-1-1z",
  requests: "M3 12h3.6l2.4-6.5 4 13 2.6-6.5H21",
  clients: "M12 12a3.6 3.6 0 1 0 0-7.2 3.6 3.6 0 0 0 0 7.2zM4.8 19.6c.7-3.4 3.4-5.2 7.2-5.2s6.5 1.8 7.2 5.2",
  admin: "M12 3.8 3.6 8.3 12 12.8l8.4-4.5zM3.6 12.2 12 16.7l8.4-4.5M3.6 16 12 20.5l8.4-4.5",
  gateways: "M5 4.8h14a1 1 0 0 1 1 1v4.4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1V5.8a1 1 0 0 1 1-1zM5 12.8h14a1 1 0 0 1 1 1v4.4a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1v-4.4a1 1 0 0 1 1-1zM7.6 8h.01M7.6 16h.01",
  history: "M12 20a8 8 0 1 0 0-16 8 8 0 0 0 0 16zM12 7.6V12l3 1.8",
  plus: "M12 5v14M5 12h14",
  x: "M6 6l12 12M18 6 6 18",
  copy: "M9 9h9a1 1 0 0 1 1 1v9a1 1 0 0 1-1 1H9a1 1 0 0 1-1-1v-9a1 1 0 0 1 1-1zM5 15V6a1 1 0 0 1 1-1h9",
  check: "M5 12.6l4.4 4.4L19 7.4",
  search: "M11 18a7 7 0 1 0 0-14 7 7 0 0 0 0 14zM16.2 16.2 20 20",
  refresh: "M19.5 11A7.6 7.6 0 1 0 17 16.7M19.8 4.6V11h-6.4",
  chevron: "M9 6l6 6-6 6",
  logout: "M10 5H6.2a1 1 0 0 0-1 1v12a1 1 0 0 0 1 1H10M15 8l4 4-4 4M19 12H9.4",
  alert: "M12 4.2 3.2 19.4h17.6zM12 10v4.2M12 16.9h.01",
  shield: "M12 3.6 5 6.2v5.4c0 4.2 2.8 7.2 7 9 4.2-1.8 7-4.8 7-9V6.2zM9 12.2l2.2 2.2 4.3-4.4",
  key: "M15.2 9.6a3.6 3.6 0 1 1-7.2 0 3.6 3.6 0 0 1 7.2 0zM11.6 13.2V20M11.6 17h2.6M11.6 19.6h1.8",
  inbox: "M4 13.5 6.4 6a1 1 0 0 1 1-.7h9.2a1 1 0 0 1 1 .7l2.4 7.5V18a1 1 0 0 1-1 1H5a1 1 0 0 1-1-1zM4 13.5h4.4a1.8 1.8 0 0 0 1.7 1.3h3.8a1.8 1.8 0 0 0 1.7-1.3H20",
  arrow: "M7 17 17 7M9 7h8v8",
  arrowdown: "M7 7l10 10M17 9v8H9",
  down: "M6 9l6 6 6-6",
};
function icon(name, size = 16) {
  return svg("svg", { width: size, height: size, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "1.6", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" },
    svg("path", { d: ICONS[name] || "" }));
}
function brandMark() {
  return svg("svg", { width: 16, height: 16, viewBox: "0 0 24 24", fill: "none", stroke: "currentColor", "stroke-width": "2.2", "stroke-linecap": "round", "stroke-linejoin": "round", "aria-hidden": "true" },
    svg("path", { d: "M5 8.6 12 5l7 3.6M5 13.2 12 9.6l7 3.6M5 17.8 12 14.2l7 3.6" }));
}

// ---- formatting ----------------------------------------------------------------------------

const nf = new Intl.NumberFormat("en-US");
const fmtInt = (n) => (n === null || n === undefined ? "—" : nf.format(n));
function fmtCompact(n) {
  if (n === null || n === undefined) return "—";
  const a = Math.abs(n);
  if (a >= 1e9) return (n / 1e9).toFixed(a >= 1e10 ? 0 : 1).replace(/\.0$/, "") + "B";
  if (a >= 1e6) return (n / 1e6).toFixed(a >= 1e7 ? 0 : 1).replace(/\.0$/, "") + "M";
  if (a >= 1e4) return (n / 1e3).toFixed(0) + "K";
  if (a >= 1e3) return nf.format(Math.round(n));
  return String(Math.round(n * 10) / 10);
}
const fmtPct = (n) => (n === null || n === undefined ? "—" : (n < 10 && n > 0 ? n.toFixed(1) : Math.round(n)) + "%");
function fmtDur(ms) {
  if (ms === null || ms === undefined) return "—";
  return ms < 1000 ? Math.round(ms) + " ms" : (ms / 1000).toFixed(ms < 10000 ? 2 : 1) + " s";
}
function fmtUsd(n) {
  if (n === null || n === undefined) return "—";
  if (n === 0) return "$0.00";
  if (n < 0.01) return "$" + n.toPrecision(2);
  return "$" + n.toLocaleString("en-US", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
}
function fmtUsdCompact(n) {
  if (n >= 1e6) return "$" + (n / 1e6).toFixed(1).replace(/\.0$/, "") + "M";
  if (n >= 1e4) return "$" + (n / 1e3).toFixed(0) + "K";
  if (n >= 1e3) return "$" + nf.format(Math.round(n));
  if (n >= 10) return "$" + Math.round(n);
  if (n >= 1) return "$" + n.toFixed(1).replace(/\.0$/, "");
  return n === 0 ? "$0" : "$" + n.toFixed(2);
}
// Axis ticks share one style: whole dollars, or two decimals when the step is a fraction.
function fmtUsdAxis(n) {
  if (n >= 1000) return fmtUsdCompact(n);
  return Number.isInteger(n) ? "$" + n : "$" + n.toFixed(2);
}
function fmtTime(iso) {
  const d = new Date(iso);
  return isNaN(d) ? String(iso) : d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit", second: "2-digit" });
}
function ago(iso) {
  const d = new Date(iso);
  if (isNaN(d)) return String(iso);
  const s = (Date.now() - d.getTime()) / 1000;
  if (s < 45) return "just now";
  if (s < 3600) return Math.round(s / 60) + " min ago";
  if (s < 86400) return Math.round(s / 3600) + " h ago";
  if (s < 86400 * 6) return Math.round(s / 86400) + " d ago";
  return d.toLocaleDateString(undefined, { month: "short", day: "numeric" });
}
const initial = (name) => (String(name || "?").replace(/^(oidc:|oidc-hash:)/, "").trim()[0] || "?");

// ---- toasts, tooltip -----------------------------------------------------------------------

function toast(message, kind) {
  const root = $("toasts");
  if (!root) return;
  const el = h("div", { class: "toast" + (kind === "bad" ? " bad" : ""), role: "status" }, icon(kind === "bad" ? "alert" : "check", 16), h("span", {}, message));
  root.append(el);
  setTimeout(() => el.remove(), kind === "bad" ? 7000 : 3800);
}
async function copyText(text, what) {
  try {
    await navigator.clipboard.writeText(text);
    toast((what || "Value") + " copied");
  } catch (_) {
    toast("Select the text and copy it by hand", "bad");
  }
}
function copyButton(text, what) {
  return h("button", { type: "button", class: "btn icon small ghost", title: "Copy " + (what || "value"), "aria-label": "Copy " + (what || "value"), onclick: () => copyText(text, what) }, icon("copy", 14));
}

const tip = (() => {
  let el = null;
  function ensure() {
    if (!el) { el = h("div", { class: "tip", role: "tooltip", hidden: true }); document.body.append(el); }
    return el;
  }
  return {
    show(x, y, title, rows) {
      const t = ensure();
      t.replaceChildren(h("div", { class: "t" }, title), ...rows.map((r) => h("div", { class: "r" }, h("span", {}, r.key ? h("i") : null, r.label), h("b", {}, r.value))));
      t.hidden = false;
      const w = t.offsetWidth, hgt = t.offsetHeight;
      let left = x + 14, top = y - hgt - 12;
      if (left + w > window.innerWidth - 8) left = x - w - 14;
      if (top < 8) top = y + 16;
      t.style.left = Math.max(8, left) + "px";
      t.style.top = top + "px";
    },
    hide() { if (el) el.hidden = true; },
  };
})();

// ---- sheets (slide-over panels) ------------------------------------------------------------

// `sticky` keeps the panel open against Escape and a click outside: a secret is shown here once.
function openSheet({ title, subtitle, body, sticky, onclose }) {
  const returnTo = document.activeElement;
  const scrim = h("div", { class: "scrim" });
  const closeBtn = h("button", { type: "button", class: "btn icon ghost", "aria-label": "Close", onclick: () => close() }, icon("x", 16));
  const bodyEl = h("div", { class: "sheet-body" }, body);
  const panel = h("aside", { class: "sheet", role: "dialog", "aria-modal": "true", "aria-label": title, tabindex: "-1" },
    h("div", { class: "sheet-head" }, h("div", { class: "grow" }, h("h2", {}, title), subtitle ? h("div", { class: "sub" }, subtitle) : null), sticky ? null : closeBtn), bodyEl);
  let closed = false;
  function close(force) {
    if (closed || (sticky && !force)) return;
    closed = true;
    document.removeEventListener("keydown", onKey, true);
    scrim.remove(); panel.remove();
    if (returnTo && returnTo.focus && document.contains(returnTo)) returnTo.focus();
    if (onclose) onclose();
  }
  function onKey(e) {
    if (e.key === "Escape") { e.stopPropagation(); close(); return; }
    if (e.key === "Tab") {
      const f = [...panel.querySelectorAll("button, input, select, textarea, a[href], [tabindex]:not([tabindex='-1'])")].filter((x) => !x.disabled && !x.hidden && x.offsetParent !== null);
      if (!f.length) return;
      const first = f[0], last = f[f.length - 1];
      if (e.shiftKey && document.activeElement === first) { e.preventDefault(); last.focus(); }
      else if (!e.shiftKey && document.activeElement === last) { e.preventDefault(); first.focus(); }
    }
  }
  scrim.addEventListener("click", () => close());
  document.addEventListener("keydown", onKey, true);
  document.body.append(scrim, panel);
  panel.focus();
  return { close: (force) => close(force), setBody(...nodes) { bodyEl.replaceChildren(...nodes); }, element: panel };
}

// ---- small components ----------------------------------------------------------------------

function skeleton(lines = 4) {
  return h("div", { class: "skel", "aria-busy": "true", "aria-label": "Loading" }, Array.from({ length: lines }, () => h("i")));
}
function emptyState({ icon: name, title, text, action }) {
  return h("div", { class: "empty" }, h("div", { class: "ico" }, icon(name || "inbox", 20)), h("b", {}, title), text ? h("p", {}, text) : null, action || null);
}
function segmented(options, current, onchange) {
  const wrap = h("div", { class: "seg", role: "group" });
  const buttons = options.map(([value, label]) => {
    const b = h("button", { type: "button", "aria-pressed": String(value === current) }, label);
    b.addEventListener("click", () => {
      buttons.forEach((x) => x.setAttribute("aria-pressed", "false"));
      b.setAttribute("aria-pressed", "true");
      onchange(value);
    });
    return b;
  });
  wrap.append(...buttons);
  return wrap;
}
function card(title, sub, body, actions) {
  return h("section", { class: "card" }, h("div", { class: "card-head" }, h("div", { class: "grow" }, h("h2", {}, title), sub ? h("div", { class: "sub" }, sub) : null), actions || null), h("div", { class: "card-body" }, body));
}
function statusPill(outcome) {
  const bad = ["http_failure", "transport_failure", "invalid_response", "stream_failure"].includes(outcome);
  const label = { completed: "Completed", http_failure: "HTTP failure", transport_failure: "Transport failure", invalid_response: "Invalid response", stream_failure: "Stream failure", abandoned: "Abandoned" }[outcome] || outcome;
  return h("span", { class: "pill" }, h("i", { class: "dot " + (outcome === "completed" ? "good" : bad ? "bad" : "warn") }), label);
}
function tableOf(heads, rows, aligns) {
  return h("div", { class: "table-wrap" }, h("table", {}, h("thead", {}, h("tr", {}, heads.map((x, i) => h("th", { class: aligns && aligns[i] === "r" ? "num" : "" }, x)))),
    h("tbody", {}, rows)));
}
// A cost is exact only when the provider said so; `est` marks a price-list estimate and `≥` a lower bound.
function costCell(row) {
  const cost = row.cost_usd;
  if (cost === null || cost === undefined) return h("span", { class: "muted", title: "The provider reported no usage the price list could price" }, "—");
  const mark = row.cost_source === "catalog" ? h("span", { class: "est", title: "Estimate from a price list, not the provider's bill" }, "est")
    : row.cost_source === "provider_floor" ? h("span", { class: "est", title: "A lower bound: the provider reported part of the bill" }, "≥") : null;
  return h("span", {}, fmtUsd(cost), mark);
}

// ---- charts --------------------------------------------------------------------------------

// A round step (1, 2 or 5 times a power of ten) so every tick is a clean number: $0.50 not $0.38.
function niceStep(v) {
  if (!(v > 0)) return 1;
  const p = Math.pow(10, Math.floor(Math.log10(v)));
  const f = v / p;
  return (f <= 1 ? 1 : f <= 2 ? 2 : f <= 5 ? 5 : 10) * p;
}
// The axis for values up to `peak`: ticks from 0 in whole steps, the top at least two steps up.
function axisFor(peak, wanted = 4) {
  const step = niceStep(peak / wanted);
  const top = Math.max(step * 2, Math.ceil(peak / step - 1e-9) * step);
  return { top, ticks: Array.from({ length: Math.round(top / step) + 1 }, (_, i) => i * step) };
}
function yTicks(max, n) {
  return Array.from({ length: n + 1 }, (_, i) => (max * i) / n);
}
// Redraw a chart whenever its box changes width; the drawing function gets the width in pixels.
function responsive(host, draw) {
  let last = 0;
  const run = () => {
    const w = Math.floor(host.clientWidth);
    if (w < 40 || w === last) return;
    last = w;
    host.replaceChildren(draw(w));
  };
  const ro = new ResizeObserver(run);
  ro.observe(host);
  requestAnimationFrame(run);
  return host;
}

// Columns: <= 24px thick, 4px rounded tops, one baseline, hairline grid, a hover tooltip and a focus tooltip
// on every column. `data[i] = {label, short, value|null, tipRows: [{label, value}]}`.
function columnChart({ data, fmtAxis, height = 220, ariaLabel }) {
  const host = h("div", { class: "chart", role: "img", "aria-label": ariaLabel || "Column chart" });
  const m = { l: 46, r: 6, t: 8, b: 24 };
  return responsive(host, (width) => {
    const w = width, ih = height - m.t - m.b, iw = w - m.l - m.r;
    const axis = axisFor(Math.max(0, ...data.map((d) => d.value || 0)));
    const max = axis.top;
    const y = (v) => m.t + ih - (v / max) * ih;
    const slot = iw / Math.max(1, data.length);
    const bw = Math.max(2, Math.min(24, slot * 0.62));
    const root = svg("svg", { width: w, height, viewBox: `0 0 ${w} ${height}` });
    for (const t of axis.ticks) {
      root.append(svg("line", { class: "grid", x1: m.l, x2: w - m.r, y1: y(t), y2: y(t) }),
        svg("text", { class: "axis-text", x: m.l - 8, y: y(t) + 4, "text-anchor": "end" }, fmtAxis(t)));
    }
    const every = Math.max(1, Math.ceil(data.length / Math.max(2, Math.floor(iw / 64))));
    data.forEach((d, i) => {
      const cx = m.l + slot * i + slot / 2;
      const g = svg("g", { class: "slot", tabindex: "0", role: "img", "aria-label": d.label + ": " + (d.tipRows && d.tipRows[0] ? d.tipRows[0].value : "") });
      const hit = svg("rect", { class: "hit", x: m.l + slot * i, y: m.t, width: slot, height: ih + 2, rx: 4 });
      g.append(hit);
      if (d.value) {
        const top = y(d.value), r = Math.min(4, bw / 2, ih - (top - m.t));
        const x0 = cx - bw / 2, x1 = cx + bw / 2, base = m.t + ih;
        g.append(svg("path", { class: "bar", d: `M${x0} ${base}V${top + r}Q${x0} ${top} ${x0 + r} ${top}H${x1 - r}Q${x1} ${top} ${x1} ${top + r}V${base}Z` }));
      }
      const show = (e) => {
        const box = hit.getBoundingClientRect();
        tip.show(e.clientX || box.left + box.width / 2, e.clientY || box.top, d.label, d.tipRows || []);
      };
      g.addEventListener("pointermove", show);
      g.addEventListener("pointerleave", () => tip.hide());
      g.addEventListener("focus", show);
      g.addEventListener("blur", () => tip.hide());
      root.append(g);
      if (i % every === 0) root.append(svg("text", { class: "axis-text", x: cx, y: height - 6, "text-anchor": "middle" }, d.short));
    });
    return root;
  });
}

// A line with a 10% wash under it, an end dot with a surface ring, a crosshair and one tooltip. Gaps (null)
// break the line. `data[i] = {label, short, value|null, tipRows}`; the axis is 0 to `max`.
function lineChart({ data, max = 100, fmtAxis, height = 200, ariaLabel }) {
  const host = h("div", { class: "chart", role: "img", "aria-label": ariaLabel || "Line chart" });
  const m = { l: 40, r: 22, t: 10, b: 24 };
  return responsive(host, (width) => {
    const w = width, ih = height - m.t - m.b, iw = w - m.l - m.r;
    const n = Math.max(1, data.length - 1);
    const x = (i) => m.l + (data.length === 1 ? iw / 2 : (iw * i) / n);
    const y = (v) => m.t + ih - (Math.min(v, max) / max) * ih;
    const root = svg("svg", { width: w, height, viewBox: `0 0 ${w} ${height}` });
    for (const t of yTicks(max, 4)) {
      root.append(svg("line", { class: "grid", x1: m.l, x2: w - m.r, y1: y(t), y2: y(t) }), svg("text", { class: "axis-text", x: m.l - 8, y: y(t) + 4, "text-anchor": "end" }, fmtAxis(t)));
    }
    const every = Math.max(1, Math.ceil(data.length / Math.max(2, Math.floor(iw / 72))));
    data.forEach((d, i) => { if (i % every === 0) root.append(svg("text", { class: "axis-text", x: x(i), y: height - 6, "text-anchor": i === 0 ? "start" : "middle" }, d.short)); });
    let run = [], last = -1;
    const flush = () => {
      if (run.length > 1) {
        const pts = run.map(([i, v]) => `${x(i)} ${y(v)}`);
        root.append(svg("path", { class: "area", d: `M${x(run[0][0])} ${m.t + ih}L${pts.join("L")}L${x(run[run.length - 1][0])} ${m.t + ih}Z` }), svg("path", { class: "line", d: "M" + pts.join("L") }));
      }
      run = [];
    };
    data.forEach((d, i) => { if (d.value === null || d.value === undefined) flush(); else { run.push([i, d.value]); last = i; } });
    flush();
    if (last >= 0) root.append(svg("circle", { class: "dotm", cx: x(last), cy: y(data[last].value), r: 4.5 }));
    const cross = svg("line", { class: "xh", y1: m.t, y2: m.t + ih, visibility: "hidden" });
    const over = svg("rect", { x: m.l, y: m.t, width: iw, height: ih, fill: "transparent", tabindex: "0", "aria-label": "Chart values" });
    const at = (e) => {
      const r = over.getBoundingClientRect();
      const i = Math.max(0, Math.min(data.length - 1, Math.round(((e.clientX - r.left) / Math.max(1, r.width)) * n)));
      cross.setAttribute("x1", x(i)); cross.setAttribute("x2", x(i)); cross.setAttribute("visibility", "visible");
      tip.show(e.clientX, e.clientY, data[i].label, data[i].tipRows || []);
    };
    over.addEventListener("pointermove", at);
    over.addEventListener("pointerleave", () => { cross.setAttribute("visibility", "hidden"); tip.hide(); });
    root.append(cross, over);
    return root;
  });
}

// A tiny 12-point trend: de-emphasised line, the current point in the accent.
function sparkline(values, w = 92, hgt = 30) {
  const pts = values.map((v, i) => [i, v]).filter(([, v]) => v !== null && v !== undefined);
  if (pts.length < 2) return h("span");
  const max = Math.max(...pts.map(([, v]) => v)), min = Math.min(...pts.map(([, v]) => v));
  const span = max - min || 1;
  const x = (i) => 3 + ((w - 6) * i) / Math.max(1, values.length - 1);
  const y = (v) => hgt - 4 - ((hgt - 8) * (v - min)) / span;
  const lastPt = pts[pts.length - 1];
  return svg("svg", { class: "spark", width: w, height: hgt, viewBox: `0 0 ${w} ${hgt}`, "aria-hidden": "true" },
    svg("path", { d: "M" + pts.map(([i, v]) => `${x(i)} ${y(v)}`).join("L") }), svg("circle", { cx: x(lastPt[0]), cy: y(lastPt[1]), r: 3.5 }));
}

// Ranked horizontal bars: a name, a value at the tip, one hue, one baseline. `items[i] = {name, sub, display, value}`.
function rankBars(items) {
  const max = Math.max(0, ...items.map((i) => i.value || 0));
  return h("div", { class: "ranks" }, items.map((it) => {
    const fill = h("span");
    fill.style.width = max > 0 ? Math.max(1.5, (it.value / max) * 100) + "%" : "0%";
    return h("div", { class: "rank" }, h("div", { class: "name", title: it.name }, it.name, it.sub ? h("small", {}, it.sub) : null), h("div", { class: "val" }, it.display), h("div", { class: "track" }, fill));
  }));
}

// A thin segmented bar of one whole: widths proportional, a part at zero draws nothing, no text on the bar.
function segBar(parts, label) {
  const total = parts.reduce((s, p) => s + (p.value || 0), 0);
  const bar = h("div", { class: "seg-bar", role: "img", "aria-label": label || parts.map((p) => p.name + " " + fmtInt(p.value)).join(", ") });
  for (const p of parts) {
    if (!p.value || !total) continue;
    const s = h("span", { class: p.cls, title: p.name + ": " + fmtInt(p.value) + " (" + fmtPct((p.value * 100) / total) + ")" });
    s.style.width = (p.value * 100) / total + "%";
    bar.append(s);
  }
  return bar;
}
function legend(items) {
  return h("div", { class: "legend" }, items.map(([cls, name]) => h("span", { class: cls }, h("i"), name)));
}

// A chart card with the table view the accessibility rules ask for: a "Table" toggle shows the same values.
function chartCard({ title, sub, chart, columns, rows, extra }) {
  const view = h("div", {}, chart);
  let table = false;
  const toggle = h("button", { type: "button", class: "btn small ghost", "aria-pressed": "false" }, "Table");
  toggle.addEventListener("click", () => {
    table = !table;
    toggle.setAttribute("aria-pressed", String(table));
    toggle.textContent = table ? "Chart" : "Table";
    view.replaceChildren(table ? h("div", { class: "chart-table" }, tableOf(columns, rows.map((r) => h("tr", {}, r.map((c, i) => h("td", { class: i ? "num" : "" }, c)))), columns.map((_, i) => (i ? "r" : "l")))) : chart);
  });
  return h("section", { class: "card" }, h("div", { class: "card-head" }, h("div", { class: "grow" }, h("h2", {}, title), sub ? h("div", { class: "sub" }, sub) : null), toggle),
    h("div", { class: "card-body" }, extra || null, view));
}
