// The look setting: the skin both windows wear, and the picker for it in settings.

import type { Look } from "./contract.ts";
import { uid } from "./controls.ts";
import { el } from "./dom.ts";

export const LOOKS: readonly (readonly [Look, string])[] = [
  ["classic", "Classic"],
  ["holo", "Holo"],
  ["gunmetal", "Gunmetal"],
];

/** Tells the stylesheets which look `root` wears; gunmetal is the default. */
export function applyLook(root: HTMLElement, look: Look): void {
  root.dataset["look"] = look;
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
