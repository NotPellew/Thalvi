import { beforeEach, describe, expect, it } from "vitest";
import type { Look, LookStyles } from "./contract.ts";
import settingsCss from "./settings.css?raw";
import settingsLooksCss from "./settings-looks.css?raw";
import { mountSettings } from "./settings.ts";
import { LOOK_STYLES, SETTINGS, settle, stubBackend } from "./testing.ts";
import themeCss from "./theme.css?raw";

function pick(root: HTMLElement, selector: string): HTMLElement {
  const found = root.querySelector<HTMLElement>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
}

function withNeon(styles: LookStyles, neon: Record<string, string>): LookStyles {
  return { ...styles, looks: { ...styles.looks, neon: { ...styles.looks.neon, ...neon } } };
}

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

  it("wears the saved look and offers the three looks on the Overlay & sounds page", async () => {
    await open("gunmetal");
    expect(root.dataset["look"]).toBe("gunmetal");
    expect(Array.from(root.querySelectorAll(".lk")).map((choice) => choice.textContent)).toEqual(["Gunmetal", "Holo", "Neon"]);
    expect(radio("gunmetal").checked).toBe(true);
  });

  it("sets the saved look's variables before it builds the pages", async () => {
    const stub = stubBackend({ ...SETTINGS, look: "holo" });
    const when: { name: string; pageBuilt: boolean }[] = [];
    const real = root.style.setProperty.bind(root.style);
    root.style.setProperty = (name: string, value: string, priority?: string): void => {
      when.push({ name, pageBuilt: root.querySelector("section[data-page]") !== null });
      real(name, value, priority);
    };
    mountSettings(root, stub.backend);
    await settle();
    expect(root.style.getPropertyValue("--neon")).toBe("#6bf8ff");
    expect(when.find((set) => set.name === "--neon")?.pageBuilt).toBe(false);
    // The window is only shown once it wears its styles, so it never flashes unstyled.
    expect(stub.backend.settingsReady).toHaveBeenCalled();
  });

  it("switches at once when picked, applies it, and goes back when the app refuses it", async () => {
    const stub = await open();
    radio("holo").click();
    expect(root.dataset["look"]).toBe("holo");
    expect(root.style.getPropertyValue("--neon")).toBe("#6bf8ff");
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ look: "holo" });
    expect(pick(root, "section[data-page=overlay] .row[data-label=look] .applied").textContent).toBe("Applied.");

    stub.backend.saveSettings.mockRejectedValueOnce("Thalvi could not apply it.");
    radio("gunmetal").click();
    expect(root.dataset["look"]).toBe("gunmetal");
    await settle();
    expect(root.dataset["look"]).toBe("holo");
    expect(root.style.getPropertyValue("--neon")).toBe("#6bf8ff");
    expect(radio("holo").checked).toBe(true);
    expect(pick(root, "section[data-page=overlay] .row[data-label=look] .note:not([hidden])").textContent).toBe("Thalvi could not apply it.");
  });

  it("re-reads the variables when the settings changed outside this window", async () => {
    const stub = await open();
    expect(root.style.getPropertyValue("--neon")).toBe("light-dark(#007a43, #2bff88)");
    stub.backend.getLookStyles.mockResolvedValueOnce(withNeon(LOOK_STYLES, { "--neon": "#123456" }));
    stub.changedElsewhere();
    await settle();
    expect(root.style.getPropertyValue("--neon")).toBe("#123456");
  });

  it("keeps the newest styles when an older answer arrives after them", async () => {
    const stub = await open();
    const waiting: ((styles: LookStyles) => void)[] = [];
    stub.backend.getLookStyles.mockImplementation(() => new Promise<LookStyles>((resolve) => { waiting.push(resolve); }));
    stub.changedElsewhere();
    await settle();
    stub.changedElsewhere();
    await settle();
    expect(waiting.length).toBeGreaterThanOrEqual(2);

    const newest = waiting[waiting.length - 1];
    const oldest = waiting[0];
    if (!newest || !oldest) throw new Error("the styles were not read twice");
    newest(withNeon(LOOK_STYLES, { "--neon": "#222222" }));
    await settle();
    expect(root.style.getPropertyValue("--neon")).toBe("#222222");
    oldest(withNeon(LOOK_STYLES, { "--neon": "#111111" }));
    await settle();
    expect(root.style.getPropertyValue("--neon")).toBe("#222222");
  });

  it("shows the window only after the newest settings load has its styles", async () => {
    const stub = stubBackend({ ...SETTINGS, look: "neon" });
    const pending: ((styles: LookStyles) => void)[] = [];
    stub.backend.getLookStyles.mockImplementation(() => new Promise<LookStyles>((resolve) => { pending.push(resolve); }));
    mountSettings(root, stub.backend);
    await settle();
    stub.backend.getSettings.mockResolvedValueOnce({ ...SETTINGS, look: "holo" });
    stub.changedElsewhere();
    await settle();

    const [older, newer] = pending;
    if (!older || !newer) throw new Error("the styles were not read twice");
    older(LOOK_STYLES);
    await settle();
    expect(stub.backend.settingsReady).not.toHaveBeenCalled();

    newer(LOOK_STYLES);
    await settle();
    expect(root.dataset["look"]).toBe("holo");
    expect(stub.backend.settingsReady).toHaveBeenCalledTimes(1);
  });

  it("puts each heading and the rows after it in one group the looks can frame", async () => {
    await open();
    const headings = (page: string) =>
      Array.from(root.querySelectorAll(`section[data-page="${page}"] .grp`)).map((group) => group.querySelector("h4")?.textContent ?? "");
    expect(headings("overlay")).toEqual(["Overlay", "Look", "Sounds"]);
    expect(headings("listening")).toEqual([
      "Right now",
      "Device",
      "Wake words",
      "Dictation wake word",
      "Hold keys",
      "Capture limits",
    ]);
    expect(headings("commands")).toEqual(["Endings", "Quiet before acting", "Apps", "Scripts"]);
    expect(root.querySelectorAll(".grp > .grp, section > .row").length).toBe(0);
  });

  it("fills a slider up to its handle", async () => {
    await open();
    const slider = pick(root, 'section[data-page="listening"] input[type="range"]') as HTMLInputElement;
    expect(slider.style.getPropertyValue("--p")).toBe("0.474");
    slider.value = "1";
    slider.dispatchEvent(new Event("input", { bubbles: true }));
    expect(slider.style.getPropertyValue("--p")).toBe("1.000");
  });
});

describe("the settings stylesheets", () => {
  // Every colour and effect is a custom property the look model emits; a literal here would
  // bring back the drift between Settings and the overlay that the model exists to remove.
  const sheets: readonly (readonly [string, string])[] = [
    ["theme.css", themeCss],
    ["settings.css", settingsCss],
    ["settings-looks.css", settingsLooksCss],
  ];
  const colorLiteral =
    /#[0-9a-fA-F]{3,8}\b|\b(?:rgba?|hsla?|hwb|lab|lch|oklab|oklch|color|color-mix|light-dark|device-cmyk)\s*\(/;
  const cascading = new Set(["initial", "inherit", "unset", "revert", "revert-layer", "currentcolor"]);

  it.each(sheets)("%s has no literal colours in its declarations", (_name, css) => {
    const literalColors: string[] = [];
    const scratch = document.createElement("span").style;
    const declarations = css.replace(/\/\*[\s\S]*?\*\//g, "").matchAll(/(?<=[;{])\s*([-a-z]+)\s*:\s*([^;{}]+)/g);
    for (const [, property, expression] of declarations) {
      if (!property || !expression || property.startsWith("--")) continue;
      const names = expression.replace(/var\(--[\w-]+\)/g, "").match(/[a-z][\w-]*/gi) ?? [];
      const paints = /^(?:color|background|border|outline|filter|accent-color|caret-color|box-shadow|text-shadow)/.test(property);
      const namedColor = paints && names.some((name) => {
        if (cascading.has(name.toLowerCase())) return false;
        scratch.color = "";
        scratch.color = name;
        return scratch.color !== "";
      });
      if (namedColor || colorLiteral.test(expression)) literalColors.push(`${property}: ${expression.trim()}`);
    }
    expect(literalColors).toEqual([]);
  });
});
