import { beforeEach, describe, expect, it } from "vitest";
import type { Look } from "./contract.ts";
import gunmetalCss from "./look-gunmetal.css?raw";
import holoCss from "./look-holo.css?raw";
import neonCss from "./look-neon.css?raw";
import settingsCss from "./settings.css?raw";
import settingsLooksCss from "./settings-looks.css?raw";
import { mountSettings } from "./settings.ts";
import { SETTINGS, settle, stubBackend } from "./testing.ts";
import themeCss from "./theme.css?raw";

function pick(root: HTMLElement, selector: string): HTMLElement {
  const found = root.querySelector<HTMLElement>(selector);
  if (!found) throw new Error(`missing ${selector}`);
  return found;
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
    const stub = await open("gunmetal");
    expect(root.dataset["look"]).toBe("gunmetal");
    expect(Array.from(root.querySelectorAll(".lk")).map((choice) => choice.textContent)).toEqual(["Gunmetal", "Holo", "Neon"]);
    expect(radio("gunmetal").checked).toBe(true);
    expect(stub.backend.settingsReady).toHaveBeenCalled();
    // The overlay needs the system scheme while Thalvi runs, not just at start.
    expect(stub.backend.setDark).toHaveBeenCalledWith(false);
    stub.schemeChanged(true);
    await settle();
    expect(stub.backend.setDark).toHaveBeenCalledWith(true);
  });

  it("switches at once when picked, applies it, and goes back when the app refuses it", async () => {
    const stub = await open();
    radio("holo").click();
    expect(root.dataset["look"]).toBe("holo");
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ look: "holo" });
    expect(pick(root, "section[data-page=overlay] .row[data-label=look] .applied").textContent).toBe("Applied.");

    stub.backend.saveSettings.mockRejectedValueOnce("Thalvi could not apply it.");
    radio("gunmetal").click();
    expect(root.dataset["look"]).toBe("gunmetal");
    await settle();
    expect(root.dataset["look"]).toBe("holo");
    expect(radio("holo").checked).toBe(true);
    expect(pick(root, "section[data-page=overlay] .row[data-label=look] .note:not([hidden])").textContent).toBe("Thalvi could not apply it.");
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
      "Keys",
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
  // Every colour and effect is a fixed custom property; a literal in a rule would bring back
  // the drift between Settings and the overlay that the shared palettes exist to remove.
  const sheets: readonly (readonly [string, string])[] = [
    ["theme.css", themeCss],
    ["settings.css", settingsCss],
    ["settings-looks.css", settingsLooksCss],
    ["look-neon.css", neonCss],
    ["look-holo.css", holoCss],
    ["look-gunmetal.css", gunmetalCss],
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

  const used = new Set(
    [themeCss, settingsCss, settingsLooksCss]
      .flatMap((css) => Array.from(css.matchAll(/var\((--[\w-]+)/g), (match) => match[1]))
      .filter((name) => name !== "--p"),
  );

  it.each([
    ["neon", neonCss, "light-dark(#007a43, #2bff88)"],
    ["holo", holoCss, "#6bf8ff"],
    ["gunmetal", gunmetalCss, "#6bf8ff"],
  ] as const)("the %s stylesheet defines every property the rules read", (look, css, accent) => {
    const defined = new Set(Array.from(css.matchAll(/^\s*(--[\w-]+):/gm), (match) => match[1]));
    expect(defined.size).toBeGreaterThan(0);
    for (const name of used) {
      expect(defined.has(name), `${look} has no ${String(name)}`).toBe(true);
    }
    expect(css).toContain(`--neon: ${accent};`);
  });
});
