// Runs the embedded operator page (src/ui/index.html) against a tiny fake DOM and a fake fetch,
// then prints a JSON report. Used by tests/ui.rs; needs only `node`, no packages.
//
//   node fake_dom.js <index.html> <scenario.json>
//
// scenario: { routes: { "<path>": { status, body } }, hostile: ["fragment", ...] }
"use strict";
const fs = require("fs");

const html = fs.readFileSync(process.argv[2], "utf8");
const scenario = JSON.parse(fs.readFileSync(process.argv[3], "utf8"));
const script = html.slice(html.indexOf("<script>") + 8, html.lastIndexOf("</script>"));

class Node_ {}
class Text extends Node_ {
  constructor(t) { super(); this.nodeType = 3; this.data = String(t); }
}
const doc = { activeElement: null, hidden: false, listeners: {}, byId: {}, dialogs: [] };

class El extends Node_ {
  constructor(tag) {
    super();
    this.nodeType = 1;
    this.tagName = String(tag).toLowerCase();
    this.children = [];
    this.attrs = {};
    this.listeners = {};
    this.hidden = false; this.disabled = false; this.checked = false; this.open = false;
    this.value = ""; this.style = {};
    this.onchange = null; this.onprofile = null;
  }
  get className() { return this.attrs["class"] || ""; }
  set className(v) { this.attrs["class"] = String(v); }
  get textContent() { return this.children.map((c) => (c instanceof Text ? c.data : c.textContent)).join(""); }
  set textContent(v) { this.children = []; if (v !== "" && v != null) this.children.push(new Text(v)); }
  appendChild(c) { this.children.push(c); return c; }
  removeChild(c) { this.children = this.children.filter((x) => x !== c); return c; }
  setAttribute(k, v) { this.attrs[k] = String(v); }
  getAttribute(k) { return Object.prototype.hasOwnProperty.call(this.attrs, k) ? this.attrs[k] : null; }
  removeAttribute(k) { delete this.attrs[k]; }
  addEventListener(t, f) { (this.listeners[t] = this.listeners[t] || []).push(f); }
  focus() { doc.activeElement = this; }
  select() {}
  showModal() { this.open = true; }
  close() { this.open = false; (this.listeners.close || []).forEach((f) => f({})); }
  querySelectorAll() { return []; }
  walk(fn) { fn(this); this.children.forEach((c) => (c instanceof El ? c.walk(fn) : fn(c))); }
}

// Static elements: every id in the HTML, with the right tag.
for (const m of html.slice(0, html.indexOf("<script>")).matchAll(/<(\w+)[^>]*\sid="([^"]+)"/g)) {
  const el = new El(m[1]);
  doc.byId[m[2]] = el;
  if (m[1] === "dialog") doc.dialogs.push(el);
}
doc.getElementById = (id) => doc.byId[id] || null;
doc.createElement = (t) => new El(t);
doc.createTextNode = (t) => new Text(t);
doc.addEventListener = (t, f) => { (doc.listeners[t] = doc.listeners[t] || []).push(f); };
doc.body = new El("body");
doc.querySelectorAll = (sel) => {
  if (sel === "dialog") return doc.dialogs;
  const out = [];
  if (sel === "[data-k]") {
    doc.byId["devices"].walk((n) => { if (n instanceof El && n.getAttribute("data-k") !== null) out.push(n); });
  }
  return out;
};

const calls = [];
const fetchFn = (path, opt) => {
  opt = opt || {};
  calls.push({ method: opt.method || "GET", path, body: opt.body === undefined ? null : opt.body });
  const route = scenario.routes[path] || { status: 404, body: { error: "unknown route" } };
  const text = typeof route.body === "string" ? route.body : JSON.stringify(route.body);
  return Promise.resolve({
    ok: route.status >= 200 && route.status < 300,
    status: route.status,
    statusText: "x",
    text: () => Promise.resolve(text),
  });
};
const navigator_ = { clipboard: { writeText: () => Promise.resolve() } };
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fire = (el, type) => {
  const ev = { type, preventDefault() {}, target: el };
  (el.listeners[type] || []).forEach((f) => f(ev));
};
const byKey = (k) => {
  let hit = null;
  doc.byId["devices"].walk((n) => { if (n instanceof El && n.getAttribute("data-k") === k) hit = n; });
  return hit;
};

const ALLOWED_TAGS = new Set(["section", "div", "h2", "h3", "span", "strong", "dl", "dt", "dd", "code", "button", "p",
  "label", "input", "table", "caption", "thead", "tbody", "tr", "th", "td", "details", "summary", "option", "textarea"]);
const ALLOWED_ATTRS = new Set(["class", "type", "role", "aria-label", "aria-hidden", "aria-pressed", "data-k", "min", "max",
  "step", "value", "scope", "tabindex"]);
const BADCHARS = /[<>"'`]/;

(async () => {
  const fn = new Function("document", "fetch", "navigator", "setInterval", "clearInterval", "setTimeout", "clearTimeout",
    "Node", script);
  fn(doc, fetchFn, navigator_, () => 1, () => {}, setTimeout, clearTimeout, Node_);
  await sleep(150);

  const report = { violations: [], interactions: {} };
  const roots = Object.values(doc.byId);
  const seen = new Set();
  for (const root of roots) {
    root.walk((n) => {
      if (!(n instanceof El) || seen.has(n)) return;
      seen.add(n);
      if (n.children.length === 0 && Object.keys(n.attrs).length === 0) return;
      const isStatic = roots.includes(n);
      if (!isStatic && !ALLOWED_TAGS.has(n.tagName)) report.violations.push("tag " + n.tagName);
      if (isStatic) return;
      for (const [k, v] of Object.entries(n.attrs)) {
        if (!ALLOWED_ATTRS.has(k)) report.violations.push("attr " + k);
        for (const frag of scenario.hostile) if (v.includes(frag)) report.violations.push("hostile in attr " + k + ": " + v);
        if (k !== "aria-label" && k !== "class" && BADCHARS.test(v)) report.violations.push("odd attr value " + k + "=" + v);
      }
    });
  }
  report.devicesText = doc.byId["devices"].textContent;
  report.cards = doc.byId["devices"].children.length;
  report.queueText = doc.byId["queue-text"].textContent;
  report.queueHidden = doc.byId["queue-panel"].hidden;
  report.profilesRows = doc.byId["profiles-rows"].children.length;
  report.errorBanner = doc.byId["banner-error"].textContent;
  report.initialCalls = calls.map((c) => c.method + " " + c.path);

  // ---- interactions through the real handlers ----
  const mark = () => calls.length;
  let m = mark();
  const toner = byKey("mono:f:toner_low");
  if (toner) { fire(toner, "click"); await sleep(60); }
  report.interactions.fault = calls.slice(m).filter((c) => c.method === "POST");

  m = mark();
  const instant = byKey("mono:t:instant");
  if (instant) { fire(instant, "click"); await sleep(60); }
  report.interactions.timing = calls.slice(m).filter((c) => c.method === "POST");

  m = mark();
  const slider = byKey("mono:consumables:0");
  if (slider) { slider.value = "15"; fire(slider, "input"); fire(slider, "change"); await sleep(60); }
  report.interactions.levels = calls.slice(m).filter((c) => c.method === "POST");

  m = mark();
  fire(byKey("mono:reset"), "click"); await sleep(60);
  report.interactions.reset = calls.slice(m).filter((c) => c.method === "POST");

  // Add form with a hostile id: refused client-side, nothing is sent.
  m = mark();
  doc.byId["add-profile"].value = "generic-wsd";
  doc.byId["add-id"].value = "../x";
  doc.byId["add-uuid"].value = ""; doc.byId["add-name"].value = "";
  fire(doc.byId["form-add"], "submit"); await sleep(60);
  report.interactions.addBad = calls.slice(m).filter((c) => c.method === "POST");
  report.interactions.addBadError = doc.byId["add-err"].textContent;

  // Add form with a good id.
  m = mark();
  doc.byId["add-id"].value = "";
  fire(doc.byId["form-add"], "submit"); await sleep(60);
  report.interactions.addGood = calls.slice(m).filter((c) => c.method === "POST");

  // Remove with "forget".
  const rm = byKey("mono:remove");
  fire(rm, "click");
  report.interactions.removeText = doc.byId["remove-text"].textContent;
  doc.byId["remove-forget"].checked = true;
  m = mark();
  fire(doc.byId["form-remove"], "submit"); await sleep(60);
  report.interactions.remove = calls.slice(m).filter((c) => c.method === "DELETE");

  m = mark();
  fire(doc.byId["btn-skip"], "click"); await sleep(60);
  fire(doc.byId["btn-advance"], "click"); await sleep(60);
  fire(doc.byId["btn-win"], "click"); await sleep(60);
  report.interactions.buttons = calls.slice(m).filter((c) => c.method === "POST");

  report.allPaths = calls.map((c) => c.path);
  console.log(JSON.stringify(report));
})().catch((e) => { console.log(JSON.stringify({ crash: String(e && e.stack || e) })); process.exit(1); });
