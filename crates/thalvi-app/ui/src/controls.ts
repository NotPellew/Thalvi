// Form controls that write into the settings draft and report each edit.

import { el } from "./dom.ts";

/** A control and the focusable element inside it that a label points at. */
export interface Field {
  node: HTMLElement;
  field: HTMLElement;
}

export interface NumberRange {
  min: number;
  max: number;
  step: number;
  unit?: string;
}

let counter = 0;
export function uid(prefix: string): string {
  counter += 1;
  return `${prefix}-${String(counter)}`;
}

export function createControls(changed: () => void) {
  function text(
    value: string,
    set: (value: string) => void,
    options: { type?: string; placeholder?: string; list?: string; label?: string } = {},
  ): Field {
    const input = el("input", {
      id: uid("f"),
      type: options.type ?? "text",
      spellcheck: "false",
      autocomplete: "off",
    });
    if (options.placeholder) input.placeholder = options.placeholder;
    if (options.list) input.setAttribute("list", options.list);
    if (options.label) input.setAttribute("aria-label", options.label);
    input.value = value;
    input.addEventListener("input", () => {
      set(input.value);
      changed();
    });
    return { node: input, field: input };
  }

  function number(value: number, range: NumberRange, set: (value: number) => void): Field {
    const input = el("input", {
      id: uid("f"),
      type: "number",
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
    });
    input.value = String(value);
    input.addEventListener("input", () => {
      if (Number.isFinite(input.valueAsNumber)) {
        set(input.valueAsNumber);
        changed();
      }
    });
    const node = el("span", { class: "num" }, input, range.unit ? el("span", { class: "unit" }, range.unit) : "");
    return { node, field: input };
  }

  function slider(value: number, range: NumberRange, set: (value: number) => void): Field {
    const input = el("input", {
      id: uid("f"),
      type: "range",
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
    });
    input.value = String(value);
    const fill = (): void => {
      const share = (input.valueAsNumber - range.min) / (range.max - range.min);
      input.style.setProperty("--p", share.toFixed(3));
    };
    fill();
    const shown = el("output", { class: "val", for: input.id }, value.toFixed(2));
    input.addEventListener("input", () => {
      fill();
      set(input.valueAsNumber);
      shown.textContent = input.valueAsNumber.toFixed(2);
      changed();
    });
    return { node: el("span", { class: "slider" }, input, shown), field: input };
  }

  function toggle(value: boolean, set: (value: boolean) => void): Field {
    const input = el("input", { id: uid("f"), type: "checkbox", role: "switch" });
    input.checked = value;
    input.addEventListener("change", () => {
      set(input.checked);
      changed();
    });
    return { node: input, field: input };
  }

  function select<T extends string>(
    options: readonly (readonly [T, string])[],
    value: T,
    set: (value: T) => void,
    label?: string,
  ): Field {
    const input = el("select", { id: uid("f") });
    if (label) input.setAttribute("aria-label", label);
    for (const [optionValue, optionLabel] of options) {
      const option = el("option", { value: optionValue }, optionLabel);
      option.selected = optionValue === value;
      input.append(option);
    }
    input.addEventListener("change", () => {
      const chosen = options.find(([optionValue]) => optionValue === input.value);
      if (chosen) {
        set(chosen[0]);
        changed();
      }
    });
    return { node: input, field: input };
  }

  return { text, number, slider, toggle, select, touched: changed };
}

export type Controls = ReturnType<typeof createControls>;
