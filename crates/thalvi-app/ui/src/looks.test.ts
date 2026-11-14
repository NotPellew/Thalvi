import { beforeEach, describe, expect, it } from "vitest";
import type { Look } from "./contract.ts";
import { mountSettings } from "./settings.ts";
import { SETTINGS, settle, stubBackend } from "./testing.ts";

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
    await open("gunmetal");
    expect(root.dataset["look"]).toBe("gunmetal");
    expect(Array.from(root.querySelectorAll(".lk")).map((choice) => choice.textContent)).toEqual(["Gunmetal", "Holo", "Neon"]);
    expect(radio("gunmetal").checked).toBe(true);
  });

  it("switches at once when picked, applies it, and goes back when the app refuses it", async () => {
    const stub = await open();
    radio("holo").click();
    expect(root.dataset["look"]).toBe("holo");
    await settle();
    expect(stub.backend.saveSettings).toHaveBeenCalledWith({ ...SETTINGS, look: "holo" });
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
