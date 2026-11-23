// The look setting: the skin both windows wear, and the picker for it in settings.

import type { Look } from "./contract.ts";
import { uid } from "./controls.ts";
import { el } from "./dom.ts";

export const LOOKS: readonly (readonly [Look, string])[] = [
  ["gunmetal", "Gunmetal"],
  ["holo", "Holo"],
  ["neon", "Neon"],
];

/** The custom properties last set on a root, so none from the look before stays behind. */
const applied = new WeakMap<HTMLElement, readonly string[]>();

/** Tells the stylesheets which look `root` wears; gunmetal is the default. */
export function applyLook(root: HTMLElement, look: Look): void {
  root.dataset["look"] = look;
}

/** Sets one look's resolved custom properties on `root`, dropping the ones it no longer has. */
export function applyLookStyles(root: HTMLElement, styles: Record<string, string>): void {
  for (const name of applied.get(root) ?? []) {
    if (!(name in styles)) root.style.removeProperty(name);
  }
  for (const [name, value] of Object.entries(styles)) root.style.setProperty(name, value);
  applied.set(root, Object.keys(styles));
}

/** One radio per look, with a small preview of it. */
export function lookPicker(value: Look, set: (look: Look) => void): HTMLElement {
  const group = uid("look");
  const choices = LOOKS.map(([look, label]) => {
    const input = el("input", { type: "radio", name: group, value: look });
    input.checked = look === value;
    input.addEventListener("change", () => {
      if (input.checked) set(look);
    });
    return el("label", { class: "lk", "data-choice": look }, input, el("span", { class: "mini" }), label);
  });
  return el("div", { class: "looks wide" }, ...choices);
}
