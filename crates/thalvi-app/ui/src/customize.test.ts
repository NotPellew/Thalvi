import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Look, LookProperty, LookStyles, LookSurface, OverlayPreviewState, Settings } from "./contract.ts";
import { mountSettings } from "./settings.ts";
import { LOOK_STYLES, SETTINGS, settle, stubBackend } from "./testing.ts";

const STATES: readonly OverlayPreviewState[] = ["live", "quiet", "working", "pending", "done", "failed"];
const ALL: readonly Look[] = ["gunmetal", "holo", "neon"];

function prop(id: string, label: string, category: string, unit: LookProperty["unit"], looks: readonly Look[], bounds: readonly [number | null, number | null, number | null] = [null, null, null], surfaces: readonly LookSurface[] = ["settings", "overlay"]): LookProperty {
  return { id, label, category, unit, min: bounds[0], max: bounds[1], step: bounds[2], looks: [...looks], surfaces: [...surfaces] };
}

const STYLES: LookStyles = {
  properties: [
    prop("colors.accent", "Accent", "colors", "color", ALL),
    prop("colors.text", "Text", "colors", "color", ALL),
    prop("colors.state.failed", "Failed", "colors", "color", ALL),
    prop("colors.secondary", "Secondary", "colors", "color", ["neon"]),
    prop("font.family", "Font", "font", "text", ALL, [1, 64, null]),
    prop("font.scale", "Font scale", "font", "number", ALL, [0.85, 1.25, 0.05]),
    prop("border.width", "Border width", "border", "number", ["neon", "holo"], [0, 3, 1]),
    prop("border.width", "Bezel width", "border", "number", ["gunmetal"], [2, 6, 1]),
    prop("decoration.scanlines", "Scanlines", "decoration", "toggle", ["holo"], [null, null, null], ["overlay"]),
    prop("motion.pulse", "Pulse", "motion", "toggle", ALL),
  ],
  looks: LOOK_STYLES.looks,
  values: {
    gunmetal: { "colors.accent": "#00ff00", "colors.text": "#eeeeee", "colors.state.failed": "#ff0000", "font.family": "", "font.scale": 1, "border.width": 4, "motion.pulse": true },
    holo: { "colors.accent": "#6bf8ff", "colors.text": "#e6fbff", "colors.state.failed": "#ff5a6e", "font.family": "", "font.scale": 1.1, "border.width": 1, "decoration.scanlines": false, "motion.pulse": true },
    neon: { "colors.accent": "#2bff88", "colors.text": "#ffffff", "colors.state.failed": "#ff5a6e", "colors.secondary": "#ff77aa", "font.family": "Inter", "font.scale": 1, "border.width": 2, "motion.pulse": false },
  },
};

function buffer(state: OverlayPreviewState, shade: number): ArrayBuffer {
  const bytes = new Uint8ClampedArray(8 + 4 * 4);
  const view = new DataView(bytes.buffer);
  view.setUint32(0, 2, true);
  view.setUint32(4, 2, true);
  for (let at = 8; at < bytes.length; at += 4) {
    bytes[at] = shade * 37;
    bytes[at + 1] = STATES.indexOf(state) * 41;
    bytes[at + 2] = 200 - shade;
    bytes[at + 3] = 255;
  }
  return bytes.buffer;
}

const drawn = new WeakMap<HTMLCanvasElement, ImageData>();

function imageOf(canvas: HTMLCanvasElement): ImageData {
  const image = drawn.get(canvas);
  if (!image) throw new Error(`no image drawn for ${String(canvas.dataset["previewState"])}`);
  return image;
}

describe("the look's Customize section", () => {
  let root: HTMLElement;

  beforeEach(() => {
    root = document.createElement("div");
    root.id = "settings";
    document.body.replaceChildren(root);
    vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockImplementation(function (this: HTMLCanvasElement, kind: string) {
      if (kind !== "2d") return null;
      return { putImageData: (image: ImageData) => { drawn.set(this, image); } } as unknown as CanvasRenderingContext2D;
    });
    vi.stubGlobal("ImageData", class {
      constructor(readonly data: Uint8ClampedArray) {}
    });
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  interface OpenOptions {
    look?: Look;
    config?: Partial<Settings>;
    styles?: LookStyles;
  }

  async function open({ look = "holo", config = {}, styles = STYLES }: OpenOptions = {}) {
    root = document.createElement("div");
    root.id = "settings";
    document.body.replaceChildren(root);
    const stub = stubBackend({ ...SETTINGS, themes: { version: 1 }, ...config, look }, undefined, styles);
    mountSettings(root, stub.backend);
    await settle();
    stub.openPage("overlay");
    await settle();
    return stub;
  }

  function pick(selector: string): HTMLElement {
    const found = root.querySelector<HTMLElement>(selector);
    if (!found) throw new Error(`missing ${selector}`);
    return found;
  }

  function row(label: string): HTMLElement {
    return pick(`[data-cust-fields] .row[data-label="${label.toLowerCase()}"]`);
  }

  function labels(): string[] {
    return Array.from(root.querySelectorAll("[data-cust-fields] .row")).map((node) => node.querySelector(".what")?.firstElementChild?.textContent ?? "");
  }

  function change(input: HTMLInputElement, value: string): void {
    input.value = value;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  }

  it.each<{ look: Look; expected: string[] }>([
    { look: "holo", expected: ["Accent", "Text", "Failed", "Font", "Font scale", "Border width", "Scanlines", "Pulse"] },
    { look: "neon", expected: ["Accent", "Text", "Failed", "Secondary", "Font", "Font scale", "Border width", "Pulse"] },
    { look: "gunmetal", expected: ["Accent", "Text", "Failed", "Font", "Font scale", "Bezel width", "Pulse"] },
  ])("offers one control per descriptor that applies to $look", async ({ look, expected }) => {
    await open({ look });
    expect(labels()).toEqual(expected);
  });

  it("uses the descriptor's unit and bounds, not hardcoded ones", async () => {
    await open();
    expect(row("Accent").querySelector("input")?.getAttribute("type")).toBe("text");
    expect(row("Font").querySelector("input")?.getAttribute("type")).toBe("text");
    expect(row("Scanlines").querySelector('input[type="checkbox"]')).not.toBeNull();
    const border = row("Border width").querySelector<HTMLInputElement>('input[type="range"]');
    expect([border?.min, border?.max, border?.step]).toEqual(["0", "3", "1"]);
    expect((row("Accent").querySelector('[data-reset]') as HTMLButtonElement).disabled).toBe(true);

    await open({ look: "gunmetal" });
    const bezel = row("Bezel width").querySelector<HTMLInputElement>('input[type="range"]');
    expect([bezel?.min, bezel?.max, bezel?.step]).toEqual(["2", "6", "1"]);
  });

  it.each([
    { label: "Accent", act: (node: HTMLElement) => { change(node.querySelector("input") as HTMLInputElement, "#ff0000"); }, patch: { themes: { holo: { colors: { dark: { accent: "#ff0000" } } } } } },
    { label: "Font", act: (node: HTMLElement) => { change(node.querySelector("input") as HTMLInputElement, "Inter"); }, patch: { themes: { holo: { font: { family: "Inter" } } } } },
    { label: "Font scale", act: (node: HTMLElement) => { change(node.querySelector('input[type="range"]') as HTMLInputElement, "1.2"); }, patch: { themes: { holo: { font: { scale: 1.2 } } } } },
    { label: "Scanlines", act: (node: HTMLElement) => { (node.querySelector('input[type="checkbox"]') as HTMLInputElement).click(); }, patch: { themes: { holo: { decoration: { scanlines: true } } } } },
  ])("sends exactly the one-property patch for $label", async ({ label, act, patch }) => {
    const stub = await open();
    act(row(label));
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith(patch);
  });

  it.each([
    { label: "Accent", id: "colors.accent", patch: { themes: { holo: { colors: { dark: { accent: null } } } } } },
    { label: "Failed", id: "colors.state.failed", patch: { themes: { holo: { colors: { dark: { state: { failed: null } } } } } } },
    { label: "Font", id: "font.family", patch: { themes: { holo: { font: { family: null } } } } },
  ])("per-property reset of $label sends null at its file path", async ({ label, id, patch }) => {
    const stub = await open({ config: { themes: { version: 1, holo: { colors: { dark: { accent: "#ff0000", state: { failed: "#ff5a6e" } } }, font: { family: "Inter" } } } } });
    const reset = pick(`[data-reset="${id}"]`) as HTMLButtonElement;
    expect(reset.disabled).toBe(false);
    reset.click();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith(patch);
    expect(pick(`[data-reset="${id}"]`).getAttribute("aria-label")).toBe(`Reset ${label}`);
  });

  it("resets only this look and leaves other looks and settings out of the patch", async () => {
    const stub = await open({ config: { themes: { version: 1, neon: { colors: { dark: { accent: "#123456" } } }, holo: { colors: { dark: { accent: "#ff0000" } } } } } });
    (pick("[data-reset-look]") as HTMLButtonElement).click();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledTimes(1);
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ themes: { holo: null } });
  });

  it.each(["themes.holo.colors.dark.text: too little contrast", "Read-only file system (os error 30)"])(
    "shows %s beside the field and puts the last applied value back",
    async (reason) => {
      const stub = await open();
      stub.backend.saveSettings.mockRejectedValueOnce(reason);
      const field = row("Text");
      const input = field.querySelector("input") as HTMLInputElement;
      const applied = input.value;
      expect(applied).toBe("#e6fbff");
      change(input, "#123456");
      await settle();
      expect(field.querySelector("p.note:not([hidden])")?.textContent).toBe(reason);
      expect(input.value).toBe(applied);
    },
  );

  it("shows the suspended banner, hides Customize, and turns styling back on", async () => {
    const stub = await open({ config: { themes: { version: 1, holo: { suspended: true, colors: { dark: { accent: "#ff0000" } } } } } });
    expect(pick("[data-cust-banner]").textContent).toContain("Custom styling is off for Holo");
    expect((pick("[data-cust-fields]")).hidden).toBe(true);
    expect(root.querySelector("[data-reset-look]")).toBeNull();
    expect(root.querySelector("[data-cust-preview]")).not.toBeNull();
    (pick("[data-banner-on]") as HTMLButtonElement).click();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ themes: { holo: { suspended: null } } });
  });

  it("shows the unreadable banner, hides Customize, and can reset the look", async () => {
    const stub = await open({ config: { themes: { version: 1, holo: { unreadable: "the file is locked" } } } });
    expect(pick("[data-cust-banner]").textContent).toContain("Your Holo customizations could not be read (the file is locked).");
    expect((pick("[data-cust-fields]")).hidden).toBe(true);
    expect(root.querySelector("[data-banner-on]")).toBeNull();
    (pick("[data-banner-reset]") as HTMLButtonElement).click();
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ themes: { holo: null } });
  });

  it("shows the reason when a banner action is refused", async () => {
    const stub = await open({ config: { themes: { version: 1, holo: { unreadable: "the file is locked" } } } });
    stub.backend.saveSettings.mockRejectedValueOnce("Read-only file system (os error 30)");
    (pick("[data-banner-reset]") as HTMLButtonElement).click();
    await settle();
    expect(pick("[data-cust-banner] p.note:not([hidden])").textContent).toBe("Read-only file system (os error 30)");
  });

  it("draws the bytes each state's preview returns into its canvas", async () => {
    const stub = await open();
    const buffers = new Map(STATES.map((state, index) => [state, buffer(state, index + 1)]));
    stub.backend.previewOverlay.mockImplementation((state: OverlayPreviewState) => Promise.resolve(buffers.get(state) as ArrayBuffer));
    stub.changedElsewhere();
    await settle();

    for (const state of STATES) {
      const canvas = pick(`canvas[data-preview-state="${state}"]`) as HTMLCanvasElement;
      const image = imageOf(canvas);
      const expected = new Uint8Array(buffers.get(state) as ArrayBuffer, 8);
      expect(new Uint8Array(image.data.buffer, image.data.byteOffset, image.data.byteLength)).toEqual(expected);
      expect([canvas.width, canvas.height, canvas.style.width, canvas.style.height]).toEqual([2, 2, "2px", "2px"]);
    }
  });

  it("keeps a newer look's preview when an older reply arrives after it", async () => {
    const stub = await open({ look: "neon" });
    const rounds: ((answer: ArrayBuffer) => void)[][] = [];
    let pending: ((answer: ArrayBuffer) => void)[] = [];
    stub.backend.previewOverlay.mockImplementation((state: OverlayPreviewState) => new Promise<ArrayBuffer>((resolve) => {
      if (state === "live") {
        pending = [];
        rounds.push(pending);
      }
      pending.push(resolve);
    }));
    stub.changedElsewhere();
    await settle();
    expect(rounds.length).toBeGreaterThan(0);

    (pick('[data-choice="holo"] input') as HTMLInputElement).click();
    await settle();
    expect(rounds.length).toBeGreaterThan(1);

    const old = buffer("live", 9);
    const recent = buffer("live", 3);
    for (const resolve of rounds[rounds.length - 1] ?? []) resolve(recent);
    for (const earlier of rounds.slice(0, -1)) for (const resolve of earlier) resolve(old);
    await settle();

    const canvas = pick('canvas[data-preview-state="live"]') as HTMLCanvasElement;
    const image = imageOf(canvas);
    expect(new Uint8Array(image.data.buffer, image.data.byteOffset, image.data.byteLength)).toEqual(new Uint8Array(recent, 8));
  });
});
