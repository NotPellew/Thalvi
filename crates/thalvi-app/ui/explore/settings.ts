// Settings exploration for #526: fast-check drives Playwright against the vite dev server with `?fake`.

import { createServer, type ViteDevServer } from "vite";
import fc from "fast-check";
import { chromium, type Browser, type BrowserContext, type Page } from "playwright-core";

const EPOCH = new Date("2026-11-17T12:00:00Z").getTime();
const DEFAULT_SEED = 1;
const DEFAULT_RUNS = 40;
const TIME_LIMIT_MS = 150_000;
const LOCALE = "en-US";
const TIME_ZONE = "UTC";
const PINNED = "fake&at=0";
const WAKE_LABEL = "Wake model (dictation)";
const WAKE_FAULT = "fail=wake-download&slow=wake-download";
const KEYS = ["Tab", "Shift+Tab", "Enter", "Space", "Escape", "ArrowDown", "ArrowUp", "ArrowLeft", "ArrowRight"] as const;

type Key = (typeof KEYS)[number];

/** One step. A generated click picks the n-th visible control; a regression click names its control. */
type Action =
  | { kind: "click"; n: number }
  | { kind: "click"; name: string }
  | { kind: "focus"; name: string }
  | { kind: "selectWake"; model: string }
  | { kind: "type"; text: string }
  | { kind: "key"; key: Key }
  | { kind: "wait"; ms: number };

type Step = Action & {
  expect?: {
    downloadFocused?: true;
    wakeStatus?: "Downloading…" | "Ready";
    refusal?: string;
  };
};

interface Target {
  x: number;
  y: number;
  what: string;
}

interface Observation {
  focus: string;
  onBody: boolean;
  progress: string[];
  silent: string[];
  unnamed: string[];
  wake: {
    badge: string;
    option: string;
    canDownload: boolean;
    downloadFocused: boolean;
  } | null;
  now: number;
}

interface Inputs {
  seed: number;
  numRuns: number;
  path: string | undefined;
}

class Violation extends Error {}

class BadInput extends Error {}

/** The process fields this file reads. Declared here so the spec needs no `@types/node`. */
declare const process: {
  env: Record<string, string | undefined>;
  exitCode: number | undefined;
};

function integer(raw: string | undefined, name: string, fallback: number): number {
  if (raw === undefined || raw === "") return fallback;
  if (!/^\d+$/.test(raw)) throw new BadInput(`${name} must be a non-negative integer, got ${JSON.stringify(raw)}`);
  return Number(raw);
}

/** Validates the process environment once; everything after this trusts `Inputs`. */
function readInputs(env: Record<string, string | undefined>): Inputs {
  const path = env["EXPLORE_PATH"];
  return {
    seed: integer(env["EXPLORE_SEED"], "EXPLORE_SEED", DEFAULT_SEED),
    numRuns: integer(env["EXPLORE_RUNS"], "EXPLORE_RUNS", DEFAULT_RUNS),
    path: path === undefined || path === "" ? undefined : path,
  };
}

const action: fc.Arbitrary<Action> = fc.oneof(
  { weight: 40, arbitrary: fc.record({ kind: fc.constant("click" as const), n: fc.nat(255) }) },
  { weight: 15, arbitrary: fc.record({ kind: fc.constant("type" as const), text: fc.stringMatching(/^[a-z0-9:/.]{1,6}$/) }) },
  { weight: 25, arbitrary: fc.record({ kind: fc.constant("key" as const), key: fc.constantFrom(...KEYS) }) },
  { weight: 15, arbitrary: fc.record({ kind: fc.constant("wait" as const), ms: fc.constantFrom(100, 1000, 2200, 5000) }) },
);
const trace = fc.array(action, { minLength: 1, maxLength: 60, size: "max" });

/** What the properties look at. Runs in the page, so its helpers live inside it. */
function observe(): Observation {
  const textOf = (node: Node | null): string => node?.textContent?.trim() ?? "";
  const attr = (el: Element, name: string): string => el.getAttribute(name)?.trim() ?? "";
  const doc = document;
  const visible = (el: Element): boolean => el.checkVisibility() && el.getClientRects().length > 0;
  const active = doc.activeElement;
  const onBody = active === null || active === doc.body || active === doc.documentElement;
  const nameOf = (el: Element): string => {
    const labelled = attr(el, "aria-label");
    return labelled === "" ? textOf(el).slice(0, 40) : labelled;
  };
  const focus = onBody
    ? "body"
    : `${active.tagName.toLowerCase()}${active.id === "" ? "" : `#${active.id}`} "${nameOf(active)}"`;
  const progress: string[] = [];
  for (const node of doc.querySelectorAll("[role=status], .applied, .note, output, option:checked, .badge, small, p")) {
    const text = textOf(node);
    if (!visible(node)) continue;
    if (/(ing|ing [^…]*)…(\s*\d+%)?$/.test(text) || text.startsWith("Downloading") || text.startsWith("Checking")) progress.push(text);
  }
  const silent: string[] = [];
  for (const input of doc.querySelectorAll("[aria-invalid=true]")) {
    if (!visible(input)) continue;
    const stack = input.closest(".stack") ?? input.parentElement;
    const alert = stack?.querySelector("[role=alert]");
    const alertHidden = alert instanceof HTMLElement && alert.hidden;
    if (alert === null || alert === undefined || alertHidden || textOf(alert) === "") silent.push(input.id === "" ? nameOf(input) : input.id);
  }
  const unnamed: string[] = [];
  const controls = doc.querySelectorAll("button, input, select, textarea, summary, a[href], [role=button], [role=switch], [role=tab], [tabindex]:not([tabindex='-1'])");
  for (const control of controls) {
    if (!visible(control) || (control instanceof HTMLInputElement && control.type === "hidden")) continue;
    const labelledBy = attr(control, "aria-labelledby");
    const byIds = labelledBy.split(/\s+/).map((id) => textOf(doc.getElementById(id))).join(" ").trim();
    const forLabel = control.id === "" ? "" : textOf(doc.querySelector(`label[for="${CSS.escape(control.id)}"]`));
    const wrapping = textOf(control.closest("label"));
    const text = control.matches("button, summary, a, [role=button], [role=tab]") ? textOf(control) : "";
    const own = `${attr(control, "aria-label")}${attr(control, "title")}${attr(control, "placeholder")}`;
    if (byIds === "" && forLabel === "" && wrapping === "" && text === "" && own === "") {
      unnamed.push(`${control.tagName.toLowerCase()}${control.id === "" ? "" : `#${control.id}`}`);
    }
  }
  const wakeRow = doc.querySelector('[data-label="wake model (dictation)"]');
  let wake: Observation["wake"] = null;
  if (wakeRow !== null && visible(wakeRow)) {
    const download = wakeRow.querySelector(".trow.pick button");
    const canDownload = download !== null && visible(download) && !download.matches(':disabled, [aria-disabled="true"]');
    wake = {
      badge: textOf(wakeRow.querySelector(".trow.pick .badge")),
      option: textOf(wakeRow.querySelector("select option:checked")),
      canDownload,
      downloadFocused: canDownload && active === download,
    };
  }
  return { focus, onBody, progress, silent, unnamed, wake, now: Date.now() };
}

/** Scrolls the n-th visible control into view. Runs in the page. */
function target(n: number): Target | null {
  const textOf = (node: Node | null): string => node?.textContent?.trim() ?? "";
  const attr = (el: Element, name: string): string => el.getAttribute(name)?.trim() ?? "";
  const all = [...document.querySelectorAll("button, input, select, textarea, summary, a[href], label, [role=button], [role=switch], [role=tab]")]
    .filter((el) => el.checkVisibility() && el.getClientRects().length > 0 && !el.matches(":disabled"));
  const el = all.length === 0 ? undefined : all[n % all.length];
  if (el === undefined) return null;
  el.scrollIntoView({ block: "center", behavior: "instant" });
  const box = el.getBoundingClientRect();
  const labelled = attr(el, "aria-label");
  const label = labelled === "" ? textOf(el).slice(0, 40) : labelled;
  return {
    x: box.left + box.width / 2,
    y: box.top + box.height / 2,
    what: `${el.tagName.toLowerCase()}${el.id === "" ? "" : `#${el.id}`} "${label}"`,
  };
}

/** What the last press landed on, and whether that can take focus. Runs in the page. */
function pressed(): { what: string; control: boolean } {
  const node: unknown = Reflect.get(window, "__pressed");
  Reflect.deleteProperty(window, "__pressed");
  if (!(node instanceof Element)) return { what: "nothing", control: false };
  const control = node.closest("button, input, select, textarea, summary, a[href], [role=button], [role=switch], [role=tab], [tabindex]") !== null;
  return { what: node.tagName.toLowerCase(), control };
}

/** Clicks the visible control whose accessible name or text is `wanted`. Runs in the page. */
function targetNamed(wanted: string): Target | null {
  const textOf = (node: Node | null): string => node?.textContent?.trim() ?? "";
  const attr = (el: Element, name: string): string => el.getAttribute(name)?.trim() ?? "";
  const all = [...document.querySelectorAll("button, input, select, textarea, summary, a[href], [role=button], [role=switch], [role=tab]")]
    .filter((el) => el.checkVisibility() && el.getClientRects().length > 0 && !el.matches(":disabled"));
  const el = all.find((candidate) => {
    const labelled = attr(candidate, "aria-label");
    const label = labelled === "" ? textOf(candidate) : labelled;
    const row = candidate.closest(".row")?.getAttribute("data-label") ?? "";
    return label === wanted || label.startsWith(wanted) || `${label} ${row}` === wanted || `${row} ${label}` === wanted;
  });
  if (el === undefined) return null;
  el.scrollIntoView({ block: "center", behavior: "instant" });
  const box = el.getBoundingClientRect();
  const labelled = attr(el, "aria-label");
  const label = (labelled === "" ? textOf(el) : labelled).slice(0, 80);
  return { x: box.left + box.width / 2, y: box.top + box.height / 2, what: `${el.tagName.toLowerCase()} "${label}"` };
}

function describe(step: Action, what: string): string {
  switch (step.kind) {
    case "click":
      return `click ${what === "" ? "(nothing)" : what}`;
    case "focus":
      return `focus ${step.name}`;
    case "selectWake":
      return `select wake model ${step.model}`;
    case "type":
      return `type "${step.text}"`;
    case "key":
      return `press ${step.key}`;
    case "wait":
      return `wait ${String(step.ms)} ms`;
  }
}

/** Chromium moves focus off a removed or disabled control at the next real frame, which the fake clock does not drive. */
function settle(page: Page): Promise<void> {
  return page.evaluate(() => new Promise<void>((resolve) => {
    const raf = Reflect.get(window, "__raf") as (callback: () => void) => void;
    raf(() => { raf(() => { resolve(); }); });
  }));
}

interface Session {
  browser: Browser;
  base: string;
}

async function openContext(session: Session, viewport: { width: number; height: number } = { width: 1000, height: 760 }): Promise<{ context: BrowserContext; page: Page; errors: string[] }> {
  const context = await session.browser.newContext({ viewport, locale: LOCALE, timezoneId: TIME_ZONE });
  await context.addInitScript(() => {
    Reflect.set(window, "__raf", window.requestAnimationFrame.bind(window));
    document.addEventListener("pointerdown", (event) => { Reflect.set(window, "__pressed", event.target); }, true);
    let state = 0x2545f491;
    Math.random = () => {
      state = (state + 0x6d2b79f5) | 0;
      let mixed = Math.imul(state ^ (state >>> 15), 1 | state);
      mixed = (mixed + Math.imul(mixed ^ (mixed >>> 7), 61 | mixed)) ^ mixed;
      return ((mixed ^ (mixed >>> 14)) >>> 0) / 4294967296;
    };
    window.addEventListener("unhandledrejection", (event) => {
      console.error("unhandled rejection", String(event.reason));
    });
  });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on("pageerror", (error) => { errors.push(`uncaught: ${error.message}`); });
  page.on("console", (message) => {
    if (message.type() === "error" && !message.location().url.endsWith("/favicon.ico")) {
      errors.push(`console: ${message.text()} at ${message.location().url}`);
    }
  });
  page.on("response", (response) => {
    if (response.status() >= 400 && !response.url().endsWith("/favicon.ico")) errors.push(`http ${String(response.status())} ${response.url()}`);
  });
  await page.clock.install({ time: EPOCH });
  await page.clock.pauseAt(EPOCH + 1);
  return { context, page, errors };
}

/** Waits until a page control has rendered. `boot.ts` imports the fake after `load`. */
async function gotoSettings(page: Page, base: string, query: string): Promise<void> {
  await page.goto(`${base}/settings.html?${query}`, { waitUntil: "commit" });
  await page.locator("nav[aria-label='Settings pages'] button").first().waitFor({ state: "visible" });
  await page.clock.runFor(500);
  await settle(page);
}

function checkWakeStatus(wake: Observation["wake"], expected: "Downloading…" | "Ready"): void {
  const property = expected === "Ready" ? "progressEnds" : "wakeDownloadStatus";
  if (wake === null) throw new Violation(`${property}: the wake model row is hidden`);
  if (wake.badge !== expected || !wake.option.endsWith(` — ${expected}`) || wake.canDownload) {
    throw new Violation(`${property}: expected ${expected} with Download unavailable, saw badge "${wake.badge}", option "${wake.option}", Download ${wake.canDownload ? "enabled" : "unavailable"}`);
  }
}

async function play(session: Session, steps: readonly Step[], query: string, log?: string[]): Promise<void> {
  const { context, page, errors } = await openContext(session);
  try {
    await gotoSettings(page, session.base, query);
    let before = await page.evaluate(observe);
    let progressSince: number | null = null;
    for (const step of steps) {
      let what = "";
      let pressedPlain = false;
      if (step.kind === "click") {
        const hit = "name" in step ? await page.evaluate(targetNamed, step.name) : await page.evaluate(target, step.n);
        if (hit !== null) {
          await page.mouse.click(hit.x, hit.y);
          const landed = await page.evaluate(pressed);
          pressedPlain = !landed.control;
          what = pressedPlain && !hit.what.startsWith("label") ? `${hit.what} (pressed ${landed.what})` : hit.what;
        } else if ("name" in step) {
          throw new Violation(`no visible control named ${step.name}`);
        }
      } else if (step.kind === "focus") {
        await page.getByLabel(step.name, { exact: true }).focus();
      } else if (step.kind === "selectWake") {
        const picker = page.getByLabel(WAKE_LABEL, { exact: true });
        await picker.focus();
        await picker.selectOption(step.model);
      } else if (step.kind === "type") {
        await page.keyboard.type(step.text);
      } else if (step.kind === "key") {
        await page.keyboard.press(step.key);
      } else {
        await page.clock.runFor(step.ms);
      }
      await page.clock.runFor(16);
      await settle(page);
      const after = await page.evaluate(observe);
      log?.push(`${describe(step, what)}  -> focus ${after.focus}`);
      // A click is the user moving focus only when it presses something that can't take focus: a label hands focus
      // to its control, plain content takes it to the body. A layout shift during the press can put plain content there.
      // Tab and Shift+Tab stay exempt: they are the user moving focus on purpose.
      const tabbed = step.kind === "key" && step.key.endsWith("Tab");
      const userMoved = pressedPlain || tabbed;
      if (errors.length > 0) throw new Violation(`errors: ${errors.join("; ")}`);
      if (!before.onBody && after.onBody && !userMoved) throw new Violation(`focusNotDropped: ${before.focus} -> body after ${describe(step, what)}`);
      if (step.expect?.downloadFocused && !after.wake?.downloadFocused) {
        throw new Violation(`focusNotDropped: expected focus on Download after ${describe(step, what)}, saw ${after.focus}`);
      }
      progressSince = after.progress.length > 0 ? (progressSince ?? after.now) : null;
      if (progressSince !== null && after.now - progressSince > 20_000) throw new Violation(`progressEnds: ${after.progress.join(" | ")}`);
      const wake = after.wake;
      if (step.expect?.wakeStatus !== undefined) checkWakeStatus(wake, step.expect.wakeStatus);
      else if (wake?.badge === "Downloading…" || wake?.option.endsWith(" — Downloading…")) checkWakeStatus(wake, "Downloading…");
      if (after.silent.length > 0) throw new Violation(`refusalsSayWhy: ${after.silent.join(", ")}`);
      const refusal = step.expect?.refusal;
      if (refusal !== undefined && await page.getByRole("alert").filter({ hasText: refusal, visible: true }).count() === 0) {
        throw new Violation(`refusalsSayWhy: no visible alert containing "${refusal}"`);
      }
      if (after.unnamed.length > 0) throw new Violation(`controlsHaveNames: ${after.unnamed.join(", ")}`);
      before = after;
    }
  } finally {
    await context.close();
  }
}

const WAKE_START: readonly Step[] = [
  { kind: "click", name: "Listening" },
  { kind: "selectWake", model: "hey_mycroft", expect: { wakeStatus: "Downloading…" } },
];

/** Named regressions for fixed bugs. Most pin the unrelated capture status. */
const REGRESSIONS: { issue: number; title: string; query?: string; steps: Step[] }[] = [
  {
    issue: 454,
    title: "status refresh while Edit on On this computer is focused",
    query: "fake",
    steps: [{ kind: "click", name: "Edit On this computer" }, { kind: "wait", ms: 5000 }],
  },
  {
    issue: 531,
    title: "Listening, Change…, Space (hold-key recorder)",
    steps: [
      { kind: "click", name: "Listening" },
      { kind: "click", name: "Change…" },
      { kind: "key", key: "Space" },
    ],
  },
  {
    issue: 532,
    title: "History, delete an entry",
    steps: [
      { kind: "click", name: "History & recordings" },
      { kind: "click", name: "Delete the entry from" },
    ],
  },
  {
    issue: 533,
    title: "Commands, Remove an app with Space",
    steps: [
      { kind: "click", name: "Commands" },
      { kind: "click", name: "New app name" },
      { kind: "key", key: "Shift+Tab" },
      { kind: "key", key: "Space" },
    ],
  },
  {
    issue: 534,
    title: "Discard the last dictation",
    steps: [
      { kind: "click", name: "History & recordings" },
      { kind: "key", key: "Tab" },
      { kind: "click", name: "Discard last dictation" },
      { kind: "type", text: "." },
    ],
  },
  {
    issue: 535,
    title: "Check for updates",
    steps: [
      { kind: "click", name: "App" },
      { kind: "click", name: "Check for updates" },
    ],
  },
  {
    issue: 536,
    title: "speech-model Download, then the refresh when it finishes",
    steps: [
      { kind: "click", name: "Add a streaming server" },
      { kind: "click", name: "Edit On this computer" },
      { kind: "click", name: "Download" },
      { kind: "key", key: "Tab" },
      { kind: "wait", ms: 5000 },
    ],
  },
  {
    issue: 546,
    title: "Commands, Remove an ending phrase",
    steps: [
      { kind: "click", name: "Commands" },
      { kind: "click", name: "Remove “never mind”" },
    ],
  },
  {
    issue: 551,
    title: "Listening, Restore default after changing the hold key",
    steps: [
      { kind: "click", name: "Listening" },
      { kind: "click", name: "Change…" },
      { kind: "type", text: "q" },
      { kind: "click", name: "Restore default" },
    ],
  },
  {
    issue: 552,
    title: "Commands, Remove script 1",
    steps: [
      { kind: "click", name: "Commands" },
      { kind: "click", name: "Remove script 1" },
    ],
  },
  {
    issue: 579,
    title: "Clear saved key in a speech server's form",
    steps: [
      { kind: "click", name: "Recognition" },
      { kind: "click", name: "Edit LLM Gateway" },
      { kind: "click", name: "Clear saved key" },
    ],
  },
  {
    issue: 593,
    title: "wake model status and Download while a model is in flight",
    query: `${PINNED}&slow=wake-download`,
    steps: [
      ...WAKE_START,
      { kind: "wait", ms: 1000, expect: { wakeStatus: "Downloading…" } },
      { kind: "wait", ms: 1200, expect: { wakeStatus: "Ready" } },
    ],
  },
  {
    issue: 594,
    title: "retry a failed wake-model download with the keyboard",
    query: `${PINNED}&${WAKE_FAULT}`,
    steps: [
      ...WAKE_START,
      { kind: "wait", ms: 2200 },
      { kind: "focus", name: WAKE_LABEL },
      { kind: "key", key: "Tab", expect: { downloadFocused: true } },
      { kind: "key", key: "Enter", expect: { wakeStatus: "Downloading…" } },
      { kind: "wait", ms: 2200, expect: { downloadFocused: true, refusal: "Could not download the wake model: the connection timed out." } },
    ],
  },
  {
    issue: 596,
    title: "a refused save keeps keyboard focus and gives a reason",
    query: `${PINNED}&fail=save`,
    steps: [
      { kind: "click", name: "Listening" },
      { kind: "focus", name: "Wake words" },
      { kind: "key", key: "Space", expect: { refusal: "The settings could not be saved: permission denied." } },
    ],
  },
  {
    issue: 620,
    title: "error bar clears while Copied is focused",
    query: "fake",
    steps: [
      { kind: "wait", ms: 37_000 },
      { kind: "click", name: "Copy" },
      { kind: "wait", ms: 2200 },
    ],
  },
  {
    issue: 620,
    title: "Dismiss error in the footer bar",
    query: "fake",
    steps: [
      { kind: "wait", ms: 37_000 },
      { kind: "click", name: "Dismiss error" },
    ],
  },
];

function replayLine(seed: number, path: string | undefined): string {
  const seedPart = `EXPLORE_SEED=${String(seed)}`;
  const pathPart = path === undefined ? "" : ` EXPLORE_PATH=${path}`;
  return `${seedPart}${pathPart} npm run explore --prefix crates/thalvi-app/ui`;
}

function reportFailure(seed: number, path: string | null, query: string, message: string, log: string[]): string {
  const lines = [
    "Settings exploration failed.",
    `backend: ?${query}`,
    `seed: ${String(seed)}`,
    `counterexample path: ${path ?? "(none)"}`,
    `property: ${message === "" ? "(no message)" : message}`,
    "trace:",
    ...log.map((line, index) => `  ${String(index + 1)}. ${line}`),
    `replay: ${replayLine(seed, path ?? undefined)}`,
  ];
  return lines.join("\n");
}

/** The lighter-model offer's controls at the reported narrow Settings window: no label spills out of its button and no reason out of the window. */
async function offerLayout(session: Session, size: { width: number; height: number }): Promise<void> {
  const { context, page, errors } = await openContext(session, size);
  try {
    await page.goto(`${session.base}/settings.html?fake&offer`, { waitUntil: "commit" });
    await page.locator("nav[aria-label='Settings pages'] button").first().waitFor({ state: "visible" });
    await page.getByRole("navigation", { name: "Settings pages" }).getByRole("button", { name: "Recognition", exact: true }).click();
    await page.getByRole("button", { name: "Use Parakeet TDT v3 (lighter on this computer)", exact: true }).waitFor({ state: "visible" });
    await page.clock.runFor(500);
    await settle(page);
    const spill = await page.evaluate(() => {
      const bad: string[] = [];
      const row = document.querySelector(".next-dictation + div");
      if (!(row instanceof HTMLElement) || row.hidden) return ["the recovery row is missing or hidden"];
      for (const node of row.querySelectorAll<HTMLElement>("button, .note")) {
        if (!node.checkVisibility() || node.getClientRects().length === 0) continue;
        const label = `${node.tagName.toLowerCase()} "${node.textContent.trim()}"`;
        if (node.scrollHeight > node.clientHeight + 1) bad.push(`${label}: its text runs below its box`);
        if (node.scrollWidth > node.clientWidth + 1) bad.push(`${label}: its text runs past its box`);
        const rect = node.getBoundingClientRect();
        if (rect.left < -0.5 || rect.right > window.innerWidth + 0.5) bad.push(`${label}: outside the window`);
      }
      return bad;
    });
    if (errors.length > 0) throw new Violation(`errors: ${errors.join("; ")}`);
    if (spill.length > 0) throw new Violation(`#705 the offer's recovery row overflows at ${String(size.width)}×${String(size.height)}: ${spill.join("; ")}`);
  } finally {
    await context.close();
  }
}

async function regressions(session: Session): Promise<void> {
  for (const regression of REGRESSIONS) {
    const log: string[] = [];
    try {
      await play(session, regression.steps, regression.query ?? PINNED, log);
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      throw new Error(`#${String(regression.issue)} (${regression.title}) failed: ${message}\n${log.map((line, index) => `  ${String(index + 1)}. ${line}`).join("\n")}`);
    }
  }
}

function thrownMessage(error: unknown): string {
  return error instanceof Error ? error.message : "";
}

async function explore(session: Session, inputs: Inputs, query: string, start: readonly Step[] = []): Promise<void> {
  const started = Date.now();
  const property = fc.asyncProperty(trace, async (steps) => {
    await play(session, [...start, ...steps], query);
  });
  const details = await fc.check(property, {
    seed: inputs.seed,
    numRuns: inputs.numRuns,
    ...(inputs.path === undefined ? {} : { path: inputs.path }),
    endOnFailure: true,
    interruptAfterTimeLimit: TIME_LIMIT_MS,
    markInterruptAsFailure: true,
  });
  const seconds = (Date.now() - started) / 1000;
  if (details.failed || details.interrupted) {
    const steps = details.counterexample?.[0] ?? [];
    const log: string[] = [];
    let message = thrownMessage(details.errorInstance);
    try {
      await play(session, [...start, ...steps], query, log);
    } catch (error) {
      // The replay names each control; an interrupt has no property message of its own.
      if (message === "") message = thrownMessage(error);
    }
    throw new Error(reportFailure(inputs.seed, details.counterexamplePath, query, message, log));
  }
  console.log(`?${query}, seed ${String(inputs.seed)}: ${String(details.numRuns)} runs in ${seconds.toFixed(1)} s`);
}

async function serve(): Promise<{ server: ViteDevServer; base: string }> {
  const server = await createServer({ server: { host: "127.0.0.1", port: 0, strictPort: false }, logLevel: "silent" });
  await server.listen();
  const base = server.resolvedUrls?.local[0];
  if (base === undefined) throw new Error("vite did not report a local url");
  return { server, base: base.replace(/\/$/, "") };
}

async function main(): Promise<void> {
  const inputs = readInputs(process.env);
  const { server, base } = await serve();
  let browser: Browser | undefined;
  try {
    browser = await chromium.launch({ headless: true });
    const session: Session = { browser, base };
    await regressions(session);
    await offerLayout(session, { width: 613, height: 858 });
    await explore(session, inputs, "fake");
    await explore(session, inputs, `fake&${WAKE_FAULT}`, WAKE_START);
  } finally {
    await browser?.close();
    await server.close();
  }
}

main().catch((error: unknown) => {
  console.error(error instanceof Error ? error.message : String(error));
  process.exitCode = 1;
});
