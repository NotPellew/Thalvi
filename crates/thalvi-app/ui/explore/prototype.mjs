// Throwaway prototype for #526: fast-check + Playwright (fake clock) against Settings `?fake`.
// Usage: node explore.mjs <seed> <seconds> [path]   -> prints a JSON result line.
import fc from "fast-check";
import { chromium } from "playwright-core";

const URL = process.env.URL ?? "http://localhost:5526/settings.html?fake";
const CHROME = "/home/agent/.cache/ms-playwright/chromium-1254/chrome-linux64/chrome";
const T0 = new Date("2026-11-17T12:00:00Z").getTime();
const seed = Number(process.argv[2] ?? 1);
const seconds = Number(process.argv[3] ?? 60);
const replayPath = process.argv[4];

const browser = await chromium.launch({ executablePath: CHROME, headless: true, args: ["--no-sandbox"] });

const KEYS = ["Tab", "Shift+Tab", "Enter", "Space", "Escape", "ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"];
const action = fc.oneof(
  { weight: 40, arbitrary: fc.record({ kind: fc.constant("click"), n: fc.nat(255) }) },
  { weight: 15, arbitrary: fc.record({ kind: fc.constant("type"), text: fc.stringMatching(/^[a-z0-9:/.]{1,6}$/) }) },
  { weight: 25, arbitrary: fc.record({ kind: fc.constant("key"), key: fc.constantFrom(...KEYS) }) },
  { weight: 15, arbitrary: fc.record({ kind: fc.constant("wait"), ms: fc.constantFrom(100, 1000, 2200, 5000) }) },
);
const trace = fc.array(action, { minLength: 1, maxLength: 60, size: "max" });

// Runs in the page: what the properties look at.
function observe() {
  const doc = document;
  const visible = (el) => el.checkVisibility() && el.getClientRects().length > 0;
  const active = doc.activeElement;
  const onBody = active === null || active === doc.body || active === doc.documentElement;
  const name = (el) => el.getAttribute("aria-label") ?? el.textContent?.trim().slice(0, 40) ?? "";
  const focus = onBody ? "body" : `${active.tagName.toLowerCase()}${active.id ? "#" + active.id : ""} "${name(active)}"`;
  const progress = [];
  for (const node of doc.querySelectorAll("[role=status], .applied, .note, output, option:checked, .badge, small, p")) {
    const text = node.textContent?.trim() ?? "";
    if (!visible(node)) continue;
    if (/(ing|ing [^…]*)…(\s*\d+%)?$/.test(text) || /^Downloading/.test(text) || /^Checking/.test(text)) progress.push(text);
  }
  const silent = [];
  for (const input of doc.querySelectorAll("[aria-invalid=true]")) {
    if (!visible(input)) continue;
    const stack = input.closest(".stack") ?? input.parentElement;
    const alert = stack?.querySelector("[role=alert]");
    if (!alert || alert.hidden || (alert.textContent ?? "").trim() === "") silent.push(input.id || name(input));
  }
  const unnamed = [];
  for (const c of doc.querySelectorAll("button, input, select, textarea, summary, a[href], [role=button], [role=switch], [role=tab], [tabindex]:not([tabindex='-1'])")) {
    if (!visible(c) || (c instanceof HTMLInputElement && c.type === "hidden")) continue;
    const byIds = c.getAttribute("aria-labelledby")?.split(/\s+/).map((id) => doc.getElementById(id)?.textContent?.trim() ?? "").join(" ").trim() ?? "";
    const forLabel = c.id ? doc.querySelector(`label[for="${CSS.escape(c.id)}"]`)?.textContent?.trim() ?? "" : "";
    const wrapping = c.closest("label")?.textContent?.trim() ?? "";
    const text = c.matches("button, summary, a, [role=button], [role=tab]") ? (c.textContent?.trim() ?? "") : "";
    const own = [c.getAttribute("aria-label"), c.getAttribute("title"), c.getAttribute("placeholder")].map((v) => v?.trim() ?? "").join("");
    if (!(byIds || forLabel || wrapping || text || own)) unnamed.push(`${c.tagName.toLowerCase()}${c.id ? "#" + c.id : ""}`);
  }
  return { focus, onBody, progress, silent, unnamed, now: Date.now() };
}

// Runs in the page: scroll the n-th visible control into view and say where it is.
function target(n) {
  const all = [...document.querySelectorAll("button, input, select, textarea, summary, a[href], label, [role=button], [role=switch], [role=tab]")]
    .filter((el) => el.checkVisibility() && el.getClientRects().length > 0 && !el.matches(":disabled"));
  if (all.length === 0) return null;
  const el = all[n % all.length];
  el.scrollIntoView({ block: "center", behavior: "instant" });
  const r = el.getBoundingClientRect();
  const label = el.getAttribute("aria-label") ?? el.textContent?.trim().slice(0, 40) ?? "";
  return { x: r.left + r.width / 2, y: r.top + r.height / 2, what: `${el.tagName.toLowerCase()}${el.id ? "#" + el.id : ""} "${label}"` };
}

const show = (a, what) =>
  a.kind === "click" ? `click ${what}` : a.kind === "type" ? `type "${a.text}"` : a.kind === "key" ? `press ${a.key}` : `wait ${a.ms} ms`;

// Chromium moves focus off a removed or disabled control at the next real frame, which the fake clock doesn't drive.
const settle = (page) => page.evaluate(() => new Promise((r) => window.__raf(() => window.__raf(() => r()))));

class Violation extends Error {}

async function run(actions, log) {
  const context = await browser.newContext({ viewport: { width: 1000, height: 760 } });
  await context.addInitScript(() => {
    window.__raf = window.requestAnimationFrame.bind(window);
    let s = 0x2545f491;
    Math.random = () => { s = (s + 0x6d2b79f5) | 0; let t = Math.imul(s ^ (s >>> 15), 1 | s); t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t; return ((t ^ (t >>> 14)) >>> 0) / 4294967296; };
    window.addEventListener("unhandledrejection", (e) => console.error("unhandled rejection", String(e.reason)));
  });
  const page = await context.newPage();
  const errors = [];
  page.on("pageerror", (e) => errors.push(`uncaught: ${e.message}`));
  page.on("console", (m) => { if (m.type() === "error" && !m.location().url.endsWith("/favicon.ico")) errors.push(`console: ${m.text()} at ${m.location().url}`); });
  page.on("response", (r) => { if (r.status() >= 400 && !r.url().endsWith("/favicon.ico")) errors.push(`http ${r.status()} ${r.url()}`); });
  await page.clock.install({ time: T0 });
  await page.clock.pauseAt(T0 + 1);
  try {
    await page.goto(URL, { waitUntil: "load" });
    await page.clock.runFor(500);
    await settle(page);
    let before = await page.evaluate(observe);
    let progressSince = null;
    for (const a of actions) {
      let what = "";
      if (a.kind === "click") {
        const t = await page.evaluate(target, a.n);
        if (t) { what = t.what; await page.mouse.click(t.x, t.y); }
      } else if (a.kind === "type") await page.keyboard.type(a.text);
      else if (a.kind === "key") await page.keyboard.press(a.key);
      else await page.clock.runFor(a.ms);
      await page.clock.runFor(16);
      await settle(page);
      const after = await page.evaluate(observe);
      log?.push(`${show(a, what)}  -> focus ${after.focus}`);
      const moved = a.kind === "click" || (a.kind === "key" && a.key.endsWith("Tab"));
      if (errors.length) throw new Violation(`errors: ${errors.join("; ")}`);
      if (!before.onBody && after.onBody && !moved) throw new Violation(`focusNotDropped: ${before.focus} -> body after ${show(a, what)}`);
      progressSince = after.progress.length ? (progressSince ?? after.now) : null;
      if (progressSince !== null && after.now - progressSince > 20_000) throw new Violation(`progressEnds: ${after.progress.join(" | ")}`);
      if (after.silent.length) throw new Violation(`refusalsSayWhy: ${after.silent.join(", ")}`);
      if (after.unnamed.length) throw new Violation(`controlsHaveNames: ${after.unnamed.join(", ")}`);
      before = after;
    }
  } finally {
    await context.close();
  }
}

const started = Date.now();
let firstFailureAt = null;
let runs = 0;
let actionsRun = 0;
const property = fc.asyncProperty(trace, async (actions) => {
  runs++;
  actionsRun += actions.length;
  try {
    await run(actions);
  } catch (e) {
    if (e instanceof Violation && firstFailureAt === null) firstFailureAt = Date.now();
    throw e;
  }
});

const details = await fc.check(property, {
  seed,
  numRuns: 100_000,
  path: replayPath,
  endOnFailure: false,
  interruptAfterTimeLimit: seconds * 1000,
  markInterruptAsFailure: false,
});
const ended = Date.now();
const result = {
  seed,
  failed: details.failed,
  interrupted: details.interrupted,
  numRuns: details.numRuns,
  runs,
  actionsRun,
  numShrinks: details.numShrinks,
  path: details.counterexamplePath,
  findSeconds: firstFailureAt && (firstFailureAt - started) / 1000,
  shrinkSeconds: firstFailureAt && (ended - firstFailureAt) / 1000,
  totalSeconds: (ended - started) / 1000,
  error: details.errorInstance?.message,
};
if (details.counterexample) {
  const [actions] = details.counterexample;
  result.length = actions.length;
  result.trace = [];
  result.replays = [];
  for (let i = 0; i < 3; i++) {
    const log = i === 0 ? result.trace : undefined;
    try { await run(actions, log); result.replays.push("passed"); } catch (e) { result.replays.push(e.message === result.error ? "same" : e.message); }
  }
}
console.log(JSON.stringify(result, null, 1));
await browser.close();
