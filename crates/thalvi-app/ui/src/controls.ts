// Form controls that apply settings when valid and report reasons when they are not.

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

export interface ControlContext {
  typing: Set<HTMLElement>;
}

let counter = 0;
export function uid(prefix: string): string {
  counter += 1;
  return `${prefix}-${String(counter)}`;
}

export function createControls(context: ControlContext) {
  function text(
    value: string,
    set: (value: string) => void,
    options: {
      type?: string;
      placeholder?: string;
      list?: string;
      label?: string;
      apply?: (value: string) => Promise<void>;
      applied?: () => string;
    } = {},
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
    const restore = (): void => {
      const applied = options.applied?.() ?? value;
      input.value = applied;
      set(applied);
    };
    const commit = (): void => {
      if (!options.apply) {
        set(input.value);
        return;
      }
      void options.apply(input.value).catch(restore);
    };
    if (options.apply) {
      input.addEventListener("focus", () => {
        context.typing.add(input);
      });
      input.addEventListener("input", () => {
        set(input.value);
      });
      input.addEventListener("keydown", (event) => {
        if (event.key === "Enter") {
          event.preventDefault();
          commit();
          input.blur();
        }
        if (event.key === "Escape") {
          event.preventDefault();
          restore();
          input.blur();
        }
      });
      input.addEventListener("blur", () => {
        context.typing.delete(input);
        commit();
      });
    } else {
      input.addEventListener("input", () => {
        set(input.value);
      });
    }
    return { node: input, field: input };
  }

  function number(
    value: number,
    range: NumberRange,
    set: (value: number) => void,
    apply?: (value: number) => Promise<void>,
    applied?: () => number,
  ): Field {
    const input = el("input", {
      id: uid("f"),
      type: "number",
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
    });
    input.value = String(value);
    const restore = (): void => {
      const from = applied?.() ?? value;
      input.value = String(from);
      set(from);
    };
    input.addEventListener("input", () => {
      if (!Number.isFinite(input.valueAsNumber)) return;
      if (!apply) set(input.valueAsNumber);
    });
    input.addEventListener("change", () => {
      if (!Number.isFinite(input.valueAsNumber)) return;
      const next = input.valueAsNumber;
      if (apply) void apply(next).catch(restore);
      else set(next);
    });
    if (apply) {
      input.addEventListener("keydown", (event) => {
        if (event.key === "Escape") {
          event.preventDefault();
          restore();
          input.blur();
        }
      });
    }
    const node = el("span", { class: "num" }, input, range.unit ? el("span", { class: "unit" }, range.unit) : "");
    return { node, field: input };
  }

  function slider(
    value: number,
    range: NumberRange,
    set: (value: number) => void,
    apply?: (value: number) => Promise<void>,
    applied?: () => number,
  ): Field {
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
    const restore = (): void => {
      const from = applied?.() ?? value;
      input.value = String(from);
      fill();
      shown.textContent = from.toFixed(2);
      set(from);
    };
    input.addEventListener("input", () => {
      fill();
      shown.textContent = input.valueAsNumber.toFixed(2);
      if (!apply) set(input.valueAsNumber);
    });
    input.addEventListener("change", () => {
      if (!apply) return;
      void apply(input.valueAsNumber).catch(restore);
    });
    return { node: el("span", { class: "slider" }, input, shown), field: input };
  }

  function toggle(
    value: boolean,
    set: (value: boolean) => void,
    apply?: (value: boolean) => Promise<void>,
    applied?: () => boolean,
  ): Field {
    const input = el("input", { id: uid("f"), type: "checkbox", role: "switch" });
    input.checked = value;
    input.addEventListener("change", () => {
      const next = input.checked;
      set(next);
      if (apply) {
        void apply(next).catch(() => {
          const from = applied?.() ?? value;
          input.checked = from;
          set(from);
        });
      }
    });
    return { node: input, field: input };
  }

  function select<T extends string>(
    options: readonly (readonly [T, string])[],
    value: T,
    set: (value: T) => void,
    label?: string,
    apply?: (value: T) => Promise<void>,
    applied?: () => T,
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
      if (!chosen) return;
      const next = chosen[0];
      set(next);
      if (apply) {
        void apply(next).catch(() => {
          const from = applied?.() ?? value;
          input.value = from;
          set(from);
        });
      }
    });
    return { node: input, field: input };
  }

  return { text, number, slider, toggle, select };
}

export type Controls = ReturnType<typeof createControls>;
