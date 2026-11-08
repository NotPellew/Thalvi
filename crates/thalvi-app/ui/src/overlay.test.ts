import { beforeEach, describe, expect, it } from "vitest";
import type { Status } from "./contract.ts";
import { mountOverlay } from "./overlay.ts";
import { manualClock, SETTINGS, settle, stubBackend } from "./testing.ts";

let root: HTMLElement;

beforeEach(() => {
  document.body.innerHTML = '<div id="overlay"></div>';
  root = document.getElementById("overlay") as HTMLElement;
});

function pick(selector: string): HTMLElement {
  const found = root.querySelector<HTMLElement>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
}

function mount(settings = SETTINGS, clicks = true) {
  const stub = stubBackend(settings);
  stub.backend.overlayTakesClicks.mockResolvedValue(clicks);
  const clock = manualClock();
  mountOverlay(root, stub.backend, clock);
  return { ...stub, clock };
}

const SENTENCE = "Call me back";
const pending = (waitingForQuiet: boolean, pauseMs = 2500, copies = false): Status => ({
  state: "pending",
  target: "Slack",
  route: "submit",
  text: SENTENCE,
  waitingForQuiet,
  pauseMs,
  copies,
});

describe("each status", () => {
  const cases: { name: string; status: Status; hidden: boolean; label: string; target: string; draft: boolean }[] = [
    { name: "paused", status: { state: "paused" }, hidden: true, label: "", target: "", draft: false },
    { name: "listening", status: { state: "listening" }, hidden: true, label: "", target: "", draft: false },
    { name: "starting", status: { state: "starting", target: "Slack" }, hidden: false, label: "LISTENING", target: "→ Slack", draft: false },
    {
      name: "capturing",
      status: { state: "capturing", target: null, committed: "Hello", partial: "wor" },
      hidden: false,
      label: "LISTENING",
      target: "",
      draft: true,
    },
    { name: "pending, waiting for quiet", status: pending(true), hidden: false, label: "QUIET", target: "→ Slack", draft: true },
    { name: "pending, counting", status: pending(false), hidden: false, label: "SUBMIT IN 2 s", target: "→ Slack", draft: true },
    { name: "pending, short pause", status: pending(false, 700), hidden: false, label: "SUBMIT IN 1 s", target: "→ Slack", draft: true },
    { name: "pending, copying", status: pending(false, 2500, true), hidden: false, label: "COPY IN 2 s", target: "→ Slack", draft: true },
    { name: "pending, writing out", status: { state: "pending", target: "Slack", route: "write", text: SENTENCE, waitingForQuiet: false, pauseMs: 700, copies: false }, hidden: false, label: "WRITE IN 1 s", target: "→ Slack", draft: true },
    { name: "writing", status: { state: "writing", target: "Slack", text: "" }, hidden: false, label: "WRITING…", target: "→ Slack", draft: true },
    { name: "working", status: { state: "working", target: "Slack", route: "task", cancellable: false }, hidden: false, label: "WORKING", target: "→ Slack", draft: false },
    {
      name: "failed shows its headline, not the full notice",
      status: { state: "failed", notice: "Thalvi could not reach the speech server at ws://127.0.0.1:9 (connection refused). Start the Thalvi server, or fix Settings → Speech → Server address.", headline: "Can't reach speech server — Settings", causes: [], kind: "other" },
      hidden: false,
      label: "Can't reach speech server — Settings",
      target: "",
      draft: false,
    },
    { name: "loading", status: { state: "loading", notice: "Loading the speech model." }, hidden: false, label: "Loading the speech model.", target: "", draft: false },
  ];
  it.each(cases)("$name", async ({ status, hidden, label, target, draft }) => {
    const { emit } = mount();
    await settle();
    emit(status);
    expect(root.hidden).toBe(hidden);
    expect(pick(".lbl").textContent).toBe(label);
    expect(pick(".tgt").textContent).toBe(target);
    expect(pick(".draft").hidden).toBe(!draft);
  });

  const done: { name: string; route: "paste" | "submit" | "task"; target: string | null; text: string }[] = [
    { name: "pasted", route: "paste", target: "Slack", text: "✓ Pasted into Slack. Opened the docs" },
    { name: "sent", route: "submit", target: "Slack", text: "✓ Sent to Slack. Opened the docs" },
    { name: "task", route: "task", target: "Slack", text: "✓ Opened the docs" },
    { name: "no target", route: "paste", target: null, text: "✓ Opened the docs" },
  ];
  it.each(done)("done, $name", ({ route, target, text }) => {
    const { emit } = mount();
    emit({ state: "done", target, route, notice: "Opened the docs" });
    expect(root.hidden).toBe(false);
    expect(pick(".lbl").textContent).toBe(text);
  });

  it.each(["paste", "submit"] as const)("keeps a refused press visible after a successful %s", (route) => {
    const { emit } = mount();
    const refusal = "Thalvi is still delivering the last dictation, so pressing the key started nothing.";
    emit({ state: "done", target: "Editor", route, notice: `Delivered. ${refusal}` });
    expect(pick(".lbl").textContent).toContain(refusal);
    expect(pick(".module .msg").textContent).toContain(refusal);
    expect(pick(".pill").dataset["tone"]).toBe("done");
    expect(pick(".module").dataset["tone"]).toBe("done");
  });
});

describe("draft", () => {
  it("shows settled text, the live partial and a caret while capturing", () => {
    const { emit } = mount();
    emit({ state: "capturing", target: null, committed: "Hello there", partial: "wor" });
    expect(pick(".c").textContent).toBe("Hello there");
    expect(pick(".p").textContent).toBe("wor");
    expect(pick(".caret").hidden).toBe(false);
  });

  it("shows the written text as it streams in, with a caret and no ending", () => {
    const { emit } = mount();
    emit({ state: "writing", target: "Slack", text: "Hi Anna, I am" });
    expect(pick(".c").textContent).toBe("Hi Anna, I am");
    expect(pick(".p").textContent).toBe("");
    expect(pick(".end").hidden).toBe(true);
    expect(pick(".caret").hidden).toBe(false);
  });

  it("marks the ending when pending", () => {
    const { emit } = mount();
    emit(pending(true));
    expect(pick(".c").textContent).toBe(SENTENCE);
    expect(pick(".end").textContent).toBe("submit");
    expect(pick(".caret").hidden).toBe(true);
  });

  it("is hidden when overlay.draft is off, leaving the pill", async () => {
    const { emit } = mount({ ...SETTINGS, overlay: { show: true, draft: false } });
    await settle();
    emit({ state: "capturing", target: "Slack", committed: "secret", partial: null });
    expect(pick(".draft").hidden).toBe(true);
    expect(pick(".lbl").textContent).toBe("LISTENING");
  });

  it("follows a settings change at the next idle moment", async () => {
    const { emit, backend } = mount();
    await settle();
    backend.getSettings.mockResolvedValue({ ...SETTINGS, overlay: { show: true, draft: false } });
    emit({ state: "listening" });
    await settle();
    emit({ state: "capturing", target: null, committed: "x", partial: null });
    expect(pick(".draft").hidden).toBe(true);
  });
});

describe("countdown", () => {
  const bar = () => pick(".cd b").style.transform;

  it("starts when waitingForQuiet turns false and resets when it turns true again", () => {
    const { emit, clock } = mount();
    emit(pending(true));
    expect(bar()).toBe("scaleX(1)");
    clock.advance(5000);
    expect(bar()).toBe("scaleX(1)");

    emit(pending(false));
    clock.advance(1000);
    expect(bar()).toBe("scaleX(0.600)");
    expect(pick(".lbl").textContent).toBe("SUBMIT IN 1 s");

    emit(pending(true));
    clock.advance(1000);
    expect(bar()).toBe("scaleX(1)");
    expect(pick(".lbl").textContent).toBe("QUIET");

    emit(pending(false));
    clock.advance(500);
    expect(bar()).toBe("scaleX(0.800)");
  });

  it("keeps running when the same pending state is announced again", () => {
    const { emit, clock } = mount();
    emit(pending(false));
    clock.advance(1000);
    emit(pending(false));
    clock.advance(500);
    expect(bar()).toBe("scaleX(0.400)");
  });
});

describe("level bars", () => {
  const middle = () => parseFloat(/scaleY\(([\d.]+)\)/.exec((root.querySelectorAll("i")[4] as HTMLElement).style.transform)?.[1] ?? "NaN");

  it("rise with the mic level and settle when it ends", async () => {
    const { emit, level, clock } = mount();
    await settle();
    emit({ state: "capturing", target: null, committed: "", partial: null });
    for (let frame = 0; frame < 10; frame += 1) clock.advance(16);
    const quiet = middle();
    level(1);
    for (let frame = 0; frame < 10; frame += 1) clock.advance(16);
    const loud = middle();
    level(0);
    for (let frame = 0; frame < 60; frame += 1) clock.advance(16);
    expect(loud).toBeGreaterThan(0.6);
    expect(quiet).toBeLessThan(0.3);
    expect(middle()).toBeLessThan(0.3);
  });

  it("go flat during the countdown", async () => {
    const { emit, level, clock } = mount();
    await settle();
    emit({ state: "capturing", target: null, committed: "", partial: null });
    level(1);
    clock.advance(16);
    emit(pending(false));
    clock.advance(16);
    expect(middle()).toBeCloseTo(4 / 22, 2);
  });
});

describe("the cancel buttons", () => {
  it.each([
    { clicks: true, capturing: true, working: true },
    { clicks: false, capturing: false, working: false },
  ])(
    "with clicks reaching the overlay: $clicks, show while a capture can still be cancelled",
    async ({ clicks, capturing, working }) => {
      const { emit } = mount(SETTINGS, clicks);
      await settle();
      const shown = (status: Status): boolean[] => {
        emit(status);
        return [!pick(".x").hidden, !pick(".mx").hidden];
      };
      expect(shown({ state: "capturing", target: null, committed: "", partial: null })).toEqual([capturing, capturing]);
      expect(shown({ state: "writing", target: "Slack", text: "Hi" })).toEqual([working, working]);
      expect(shown({ state: "working", target: "Slack", route: "task", cancellable: true })).toEqual([working, working]);
      expect(shown({ state: "working", target: "Slack", route: "task", cancellable: false })).toEqual([false, false]);
      expect(shown({ state: "done", target: "Slack", route: "task", notice: "Done" })).toEqual([false, false]);
    },
  );
});

describe("backend calls", () => {
  it("cancel asks the backend to cancel the capture", () => {
    const { emit, backend } = mount();
    emit({ state: "starting", target: null });
    pick(".x").click();
    expect(backend.cancelCapture).toHaveBeenCalledOnce();
  });

  it("an event that arrives before the first status wins", async () => {
    const stub = stubBackend(SETTINGS, { state: "paused" });
    mountOverlay(root, stub.backend, manualClock());
    stub.emit({ state: "starting", target: "Slack" });
    await settle();
    expect(root.hidden).toBe(false);
  });
});
