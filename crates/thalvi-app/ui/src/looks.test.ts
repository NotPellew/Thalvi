import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Look, Route, Settings, Status } from "./contract.ts";
import { mountOverlay } from "./overlay.ts";
import { mountSettings } from "./settings.ts";
import { manualClock, SETTINGS, settle, stubBackend } from "./testing.ts";

function pick(root: HTMLElement, selector: string): HTMLElement {
  const found = root.querySelector<HTMLElement>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
}

describe("the overlay in the holo and gunmetal looks", () => {
  let root: HTMLElement;
  let canvasDraws = 0;

  beforeEach(() => {
    document.body.innerHTML = '<div id="overlay"></div>';
    root = pick(document.body, "#overlay");
    canvasDraws = 0;
    const context = new Proxy(
      {},
      {
        get: (_, name) => {
          if (name === "clearRect") {
            return () => {
              canvasDraws += 1;
            };
          }
          if (name === "createLinearGradient") return () => ({ addColorStop: () => undefined });
          return () => undefined;
        },
        set: () => true,
      },
    );
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(context as unknown as CanvasRenderingContext2D);
  });

  afterEach(() => {
    vi.restoreAllMocks();
  });

  async function mount(look: Look = "holo", settings: Settings = SETTINGS) {
    const stub = stubBackend({ ...settings, look });
    const clock = manualClock();
    mountOverlay(root, stub.backend, clock);
    await settle();
    return { ...stub, clock };
  }

  const module = () => pick(root, ".module");
  const label = () => pick(module(), ".label").textContent;
  const line = () => pick(module(), ".tick").textContent;
  const pending = (
    waitingForQuiet: boolean,
    pauseMs = 2500,
    wording: { route: Route; target: string | null; text: string; copies?: boolean } = { route: "submit", target: "Slack", text: "Call me back" },
  ): Status => ({ state: "pending", copies: false, ...wording, waitingForQuiet, pauseMs });

  it("wears the look from the settings and follows a change at the next idle moment", async () => {
    const { emit, saved, backend } = await mount("gunmetal");
    expect(root.dataset["look"]).toBe("gunmetal");

    emit({ state: "capturing", target: null, committed: "x", partial: null, notice: null });
    backend.getSettings.mockResolvedValue({ ...SETTINGS, look: "holo" });
    saved();
    await settle();
    expect(root.dataset["look"]).toBe("gunmetal");

    emit({ state: "listening" });
    await settle();
    expect(root.dataset["look"]).toBe("holo");

    backend.getSettings.mockResolvedValue({ ...SETTINGS, look: "neon" });
    saved();
    await settle();
    expect(root.dataset["look"]).toBe("neon");
  });

  const states: { name: string; status: Status; tone: string; label: string; tag: boolean; line: string; message: string }[] = [
    { name: "starting", status: { state: "starting", target: "Slack" }, tone: "live", label: "", tag: false, line: "", message: "" },
    {
      name: "speaking",
      status: { state: "capturing", target: null, committed: "Hello", partial: "wor", notice: null },
      tone: "live",
      label: "",
      tag: false,
      line: "Hello wor",
      message: "",
    },
    {
      name: "speaking with a notice the user should read",
      status: { state: "capturing", target: null, committed: "Hello", partial: null, notice: "Say the ending again." },
      tone: "live",
      label: "",
      tag: false,
      line: "Say the ending again.",
      message: "",
    },
    { name: "waiting for quiet", status: pending(true), tone: "quiet", label: "", tag: false, line: "Call me back", message: "" },
    { name: "countdown", status: pending(false), tone: "pending", label: "SEND TO SLACK IN 3 s", tag: true, line: "Call me back", message: "" },
    {
      name: "paste and submit countdown on a desktop that only copies",
      status: pending(false, 2500, { route: "submit", target: null, text: "Call me back", copies: true }),
      tone: "pending",
      label: "COPY IN 3 s",
      tag: true,
      line: "Call me back",
      message: "",
    },
    {
      name: "a task countdown on a desktop that only copies",
      status: pending(false, 2500, { route: "task", target: "Firefox", text: "open the calendar", copies: true }),
      tone: "pending",
      label: "TASK IN 3 s",
      tag: true,
      line: "open the calendar",
      message: "",
    },
    {
      name: "paste countdown without a target",
      status: pending(false, 700, { route: "paste", target: null, text: "Call me back" }),
      tone: "pending",
      label: "PASTE IN 1 s",
      tag: true,
      line: "Call me back",
      message: "",
    },
    { name: "working", status: { state: "working", target: "Slack", route: "task", cancellable: false }, tone: "working", label: "WORKING", tag: true, line: "", message: "" },
    {
      name: "loading",
      status: { state: "loading", notice: "Loading the speech model." },
      tone: "working",
      label: "LOADING",
      tag: true,
      line: "Loading the speech model.",
      message: "",
    },
    {
      name: "delivered",
      status: { state: "done", target: "Slack", route: "submit", notice: "Sent" },
      tone: "done",
      label: "",
      tag: false,
      line: "",
      message: "✓ Sent to Slack. Sent",
    },
    {
      name: "failed",
      status: { state: "failed", notice: "Thalvi could not reach the speech server at ws://127.0.0.1:9 (connection refused). Start the Thalvi server, or fix Settings → Speech → Server address.", headline: "Can't reach speech server — Settings", causes: [], kind: "other" },
      tone: "failed",
      label: "FAILED",
      tag: true,
      line: "Can't reach speech server — Settings",
      message: "",
    },
    {
      name: "failed delivery",
      status: { state: "failed", notice: "Thalvi could not reach the window that was open when you started.", headline: "Window closed — text on clipboard", causes: [], kind: "delivery" },
      tone: "failed",
      label: "NOT SENT",
      tag: true,
      line: "Window closed — text on clipboard",
      message: "",
    },
  ];
  it.each(states)("$name", async (expected) => {
    for (const look of ["holo", "gunmetal"] as const) {
      const { emit } = await mount(look);
      emit(expected.status);
      expect(module().dataset["tone"]).toBe(expected.tone);
      expect(pick(module(), ".tag").hidden).toBe(!expected.tag);
      expect(label()).toBe(expected.label);
      expect(line()).toBe(expected.line);
      expect(pick(module(), ".msg").textContent).toBe(expected.message);
      document.body.innerHTML = '<div id="overlay"></div>';
      root = pick(document.body, "#overlay");
    }
  });

  it("counts the pause down in whole seconds rounded up and drains the strip", async () => {
    const { emit, clock } = await mount();
    const strip = () => pick(module(), ".line").style.transform;
    emit(pending(false));
    expect(strip()).toBe("scaleX(1.000)");
    clock.advance(1000);
    expect([label(), strip()]).toEqual(["SEND TO SLACK IN 2 s", "scaleX(0.600)"]);
    clock.advance(500);
    expect(label()).toBe("SEND TO SLACK IN 1 s");
    clock.advance(1000);
    expect([label(), strip()]).toEqual(["SEND TO SLACK IN 1 s", "scaleX(0.000)"]);

    emit(pending(true));
    clock.advance(1000);
    expect(strip()).toBe("");
  });

  it("shows the pending text as it is, without the ending word", async () => {
    const { emit } = await mount();
    emit(pending(false, 2500, { route: "task", target: "Firefox", text: "open the calendar" }));
    expect(line()).toBe("open the calendar");
  });

  it("keeps the download notice scrolling as its percentage changes", async () => {
    const { emit } = await mount("gunmetal");
    emit({ state: "loading", notice: "Downloading the speech model, about 650 MB — 10%" });
    const run = pick(module(), ".tick .run");
    emit({ state: "loading", notice: "Downloading the speech model, about 650 MB — 11%" });
    expect(pick(module(), ".tick .run")).toBe(run);
    expect(run.textContent).toBe("Downloading the speech model, about 650 MB — 11%");
  });

  it("keeps the line single and slides it left only when the words overflow", async () => {
    const descriptor = Object.getOwnPropertyDescriptor(Element.prototype, "scrollWidth");
    let wide = 500;
    Object.defineProperty(Element.prototype, "scrollWidth", {
      configurable: true,
      get(this: Element) {
        return this.classList.contains("run") ? wide : 0;
      },
    });
    Object.defineProperty(Element.prototype, "clientWidth", {
      configurable: true,
      get(this: Element) {
        return this.classList.contains("tick") ? 300 : 0;
      },
    });
    try {
      const { emit } = await mount();
      const run = () => pick(module(), ".run");
      emit({ state: "capturing", target: null, committed: "a long line of words", partial: null, notice: null });
      expect(pick(module(), ".tick").classList.contains("over")).toBe(true);
      expect(run().style.transform).toBe("translateX(-200px)");

      wide = 120;
      emit({ state: "capturing", target: null, committed: "short", partial: null, notice: null });
      expect(pick(module(), ".tick").classList.contains("over")).toBe(false);
      expect(run().style.transform).toBe("translateX(0px)");

      wide = 500;
      emit({ state: "failed", notice: "A notice.", headline: "A headline longer than the line.", causes: [], kind: "other" });
      expect(run().classList.contains("scroll")).toBe(true);
      expect(run().style.getPropertyValue("--over")).toBe("232px");

      wide = 120;
      emit({ state: "failed", notice: "Short.", headline: "Short.", causes: [], kind: "other" });
      expect(run().classList.contains("scroll")).toBe(false);
    } finally {
      if (descriptor) Object.defineProperty(Element.prototype, "scrollWidth", descriptor);
      Reflect.deleteProperty(Element.prototype, "clientWidth");
    }
    expect(module().querySelectorAll(".tick .run").length).toBe(1);
  });

  it("hides the dictated words with overlay.draft off but still tells a failure", async () => {
    const { emit } = await mount("holo", { ...SETTINGS, overlay: { show: true, draft: false } });
    emit({ state: "capturing", target: "Slack", committed: "secret", partial: null, notice: null });
    expect(line()).toBe("");
    expect(module().dataset["text"]).toBe("false");
    emit({ state: "failed", notice: "Server unreachable.", headline: "Server unreachable.", causes: [], kind: "other" });
    expect(line()).toBe("Server unreachable.");
    expect(module().dataset["text"]).toBe("true");
  });

  it("cancels from its own button while a capture can still be cancelled", async () => {
    const { emit, backend } = await mount();
    const cancel = () => pick(module(), ".mx") as HTMLButtonElement;
    const shown = (status: Status): boolean => {
      emit(status);
      return !cancel().hidden;
    };
    expect(shown({ state: "capturing", target: null, committed: "", partial: null, notice: null })).toBe(true);
    cancel().click();
    expect(backend.cancelCapture).toHaveBeenCalledOnce();
    expect(shown({ state: "working", target: "Slack", route: "task", cancellable: true })).toBe(true);
    expect(shown({ state: "working", target: "Slack", route: "task", cancellable: false })).toBe(false);
    expect(shown({ state: "loading", notice: "Loading." })).toBe(false);
  });

  describe("the wave", () => {
    const run = (clock: ReturnType<typeof manualClock>, ms: number): number => {
      const before = canvasDraws;
      for (let elapsed = 0; elapsed < ms; elapsed += 1000 / 60) clock.advance(1000 / 60);
      return canvasDraws - before;
    };

    it("is drawn on every frame while the voice is live, and rarely once it has settled", async () => {
      const { emit, level, clock } = await mount();
      emit({ state: "capturing", target: null, committed: "", partial: null, notice: null });
      level(0.8);
      expect(run(clock, 1000)).toBeGreaterThan(55);

      level(0);
      run(clock, 4000);
      expect(run(clock, 1000)).toBeLessThanOrEqual(11);
    });

    it("is not drawn in the neon look, behind a label, or while the overlay is idle", async () => {
      const neon = await mount("neon");
      neon.emit({ state: "capturing", target: null, committed: "", partial: null, notice: null });
      expect(run(neon.clock, 500)).toBe(0);

      document.body.innerHTML = '<div id="overlay"></div>';
      root = pick(document.body, "#overlay");
      const { emit, level, clock } = await mount("gunmetal");
      emit({ state: "capturing", target: null, committed: "", partial: null, notice: null });
      level(0.8);
      expect(run(clock, 200)).toBeGreaterThan(0);
      emit(pending(false));
      expect(run(clock, 500)).toBe(0);
      emit({ state: "listening" });
      expect(run(clock, 500)).toBe(0);
    });
  });
});

describe("the settings window's look", () => {
  let root: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = '<div id="settings"></div>';
    root = pick(document.body, "#settings");
  });

  async function open(look: Look = "neon") {
    const stub = stubBackend({ ...SETTINGS, look });
    mountSettings(root, stub.backend);
    await settle();
    return stub;
  }

  const radio = (look: Look) => pick(root, `[data-choice="${look}"] input`) as HTMLInputElement;
  const button = (name: string) => {
    const found = Array.from(root.querySelectorAll("button")).find((candidate) => candidate.textContent === name);
    if (!found) throw new Error(`no button ${name}`);
    return found;
  };

  it("wears the saved look and offers the three looks on the Feedback and appearance page", async () => {
    await open("gunmetal");
    expect(root.dataset["look"]).toBe("gunmetal");
    expect(Array.from(root.querySelectorAll(".lk")).map((choice) => choice.textContent)).toEqual(["Gunmetal", "Holo", "Neon"]);
    expect(radio("gunmetal").checked).toBe(true);
  });

  it("switches at once when picked, saves with the other settings, and goes back on revert", async () => {
    const stub = await open();
    radio("holo").click();
    expect(root.dataset["look"]).toBe("holo");
    expect(pick(root, ".msg").textContent).toBe("Unsaved changes");

    button("Save").click();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ ...SETTINGS, look: "holo" });
    await settle();

    radio("gunmetal").click();
    button("Revert").click();
    await settle();
    expect(root.dataset["look"]).toBe("neon");
  });

  it("puts each heading and the rows after it in one group the looks can frame", async () => {
    await open();
    const headings = (page: string) =>
      Array.from(root.querySelectorAll(`section[data-page="${page}"] .grp`)).map((group) => group.querySelector("h4")?.textContent ?? "");
    expect(headings("feedback")).toEqual(["Overlay", "Look", "Sounds"]);
    expect(headings("capture")).toEqual([
      "Wake words",
      "Dictation wake word",
      "Hold keys",
      "Endings",
      "Quiet before acting",
      "Capture limits",
    ]);
    expect(root.querySelectorAll(".grp > .grp, section > .row").length).toBe(0);
  });

  it("fills a slider up to its handle", async () => {
    await open();
    const slider = pick(root, 'section[data-page="capture"] input[type="range"]') as HTMLInputElement;
    expect(slider.style.getPropertyValue("--p")).toBe("0.474");
    slider.value = "1";
    slider.dispatchEvent(new Event("input", { bubbles: true }));
    expect(slider.style.getPropertyValue("--p")).toBe("1.000");
  });
});
