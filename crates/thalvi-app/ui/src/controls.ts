// Form controls that apply their value when the user commits it, and say next to themselves how that went.

import { errorText } from "./contract.ts";
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

/** Resolves once the value is applied; rejects with the plain reason it was not. */
export type Commit<T> = (value: T) => Promise<void>;

let counter = 0;
export function uid(prefix: string): string {
  counter += 1;
  return `${prefix}-${String(counter)}`;
}

/** What a control says about its last attempt: the confirmation, or the reason it was refused. */
interface Verdict {
  applied: HTMLElement;
  problem: HTMLElement;
}

function verdict(): Verdict {
  return {
    applied: el("small", { class: "applied", role: "status" }),
    problem: el("p", { class: "note", role: "alert", hidden: true }),
  };
}

function around(control: HTMLElement, { applied, problem }: Verdict): HTMLElement {
  return el("span", { class: "stack" }, control, applied, problem);
}

/** `confirmation` is the text a control shows once its value is applied; `atOnce` is for a setting the running capture takes up too. */
export function createControls(confirmation: (atOnce?: boolean) => string) {
  /** Runs one attempt to apply; on a refusal shows the reason and calls `revert`. */
  async function attempt({ applied, problem }: Verdict, apply: Promise<void>, revert: () => void, atOnce = false): Promise<void> {
    applied.textContent = "";
    problem.hidden = true;
    try {
      await apply;
      applied.textContent = confirmation(atOnce);
    } catch (error) {
      problem.textContent = errorText(error);
      problem.hidden = false;
      revert();
    }
  }

  function escapable(input: HTMLElement & { value: string }, shown: Verdict, appliedValue: () => string): void {
    input.addEventListener("keydown", (event) => {
      if (event.key !== "Escape") return;
      input.value = appliedValue();
      input.setAttribute("aria-invalid", "false");
      shown.problem.hidden = true;
      shown.applied.textContent = "";
    });
  }

  /** A typed field (a text area with `rows`): applied when it loses focus or on Enter; Escape puts the applied value back. */
  function text(
    value: string,
    commit: Commit<string>,
    options: { type?: string; placeholder?: string; list?: string; label?: string; rows?: number } = {},
  ): Field {
    const input =
      options.rows === undefined
        ? el("input", { id: uid("f"), type: options.type ?? "text", spellcheck: "false", autocomplete: "off" })
        : el("textarea", { id: uid("f"), rows: String(options.rows), spellcheck: "false" });
    if (options.placeholder) input.placeholder = options.placeholder;
    if (options.list) input.setAttribute("list", options.list);
    if (options.label) input.setAttribute("aria-label", options.label);
    input.value = value;
    let kept = value;
    const shown = verdict();
    input.addEventListener("change", () => {
      if (input.value === kept) return;
      const wanted = input.value;
      void attempt(shown, commit(wanted).then(() => { kept = wanted; input.setAttribute("aria-invalid", "false"); }), () => {
        input.setAttribute("aria-invalid", "true");
      });
    });
    escapable(input, shown, () => kept);
    return { node: around(input, shown), field: input };
  }

  /** A number field: applied when it loses focus or on Enter, unless it is not a number in range. */
  function number(value: number, range: NumberRange, commit: Commit<number>): Field {
    const input = el("input", {
      id: uid("f"),
      type: "number",
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
    });
    input.value = String(value);
    let kept = String(value);
    const shown = verdict();
    input.addEventListener("change", () => {
      const wanted = input.valueAsNumber;
      if (input.value === kept) return;
      const unit = range.unit ? ` ${range.unit}` : "";
      const result =
        Number.isFinite(wanted) && wanted >= range.min && wanted <= range.max
          ? commit(wanted)
          : Promise.reject(new Error(`Enter a number from ${String(range.min)} to ${String(range.max)}${unit}.`));
      const typedValue = input.value;
      void attempt(shown, result.then(() => { kept = typedValue; input.setAttribute("aria-invalid", "false"); }), () => {
        input.setAttribute("aria-invalid", "true");
      });
    });
    escapable(input, shown, () => kept);
    const unit = range.unit ? el("span", { class: "unit" }, range.unit) : "";
    return { node: around(el("span", { class: "num" }, input, unit), shown), field: input };
  }

  /** A slider: the number beside it follows the thumb, and the value is applied when the thumb is let go. */
  function slider(value: number, range: NumberRange, commit: Commit<number>): Field {
    const input = el("input", {
      id: uid("f"),
      type: "range",
      min: String(range.min),
      max: String(range.max),
      step: String(range.step),
    });
    input.value = String(value);
    let kept = value;
    const fill = (): void => {
      const share = (input.valueAsNumber - range.min) / (range.max - range.min);
      input.style.setProperty("--p", share.toFixed(3));
      shown.textContent = input.valueAsNumber.toFixed(2);
    };
    const shown = el("output", { class: "val", for: input.id }, value.toFixed(2));
    const result = verdict();
    fill();
    input.addEventListener("input", fill);
    input.addEventListener("change", () => {
      const wanted = input.valueAsNumber;
      void attempt(result, commit(wanted).then(() => { kept = wanted; }), () => {
        input.value = String(kept);
        fill();
      });
    });
    return { node: around(el("span", { class: "slider" }, input, shown), result), field: input };
  }

  /** A switch: applied as it is flipped, and flipped back when refused. */
  function toggle(value: boolean, commit: Commit<boolean>, atOnce = false): Field {
    const input = el("input", { id: uid("f"), type: "checkbox", role: "switch" });
    input.checked = value;
    const shown = verdict();
    input.addEventListener("change", () => {
      const wanted = input.checked;
      void attempt(shown, commit(wanted), () => {
        input.checked = !wanted;
      }, atOnce);
    });
    return { node: around(input, shown), field: input };
  }

  /** A switch whose caller applies it and reports on it itself. */
  function bareToggle(value: boolean): Field {
    const input = el("input", { id: uid("f"), type: "checkbox", role: "switch" });
    input.checked = value;
    return { node: input, field: input };
  }

  /** A choice: applied as it is made, and put back when refused. */
  function select<T extends string>(
    options: readonly (readonly [T, string])[],
    value: T,
    commit: Commit<T>,
    label?: string,
  ): Field {
    const input = el("select", { id: uid("f") });
    if (label) input.setAttribute("aria-label", label);
    for (const [optionValue, optionLabel] of options) {
      const option = el("option", { value: optionValue }, optionLabel);
      option.selected = optionValue === value;
      input.append(option);
    }
    let kept = value;
    const shown = verdict();
    input.addEventListener("change", () => {
      const chosen = options.find(([optionValue]) => optionValue === input.value);
      if (!chosen) return;
      void attempt(shown, commit(chosen[0]).then(() => { kept = chosen[0]; }), () => {
        input.value = kept;
      });
    });
    return { node: around(input, shown), field: input };
  }

  /** A text field of a form that is applied as a whole: it only keeps what is typed. */
  function draftText(
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
    });
    return { node: input, field: input };
  }

  /** A choice of a form that is applied as a whole: it only keeps what is chosen. */
  function draftSelect<T extends string>(
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
      if (chosen) set(chosen[0]);
    });
    return { node: input, field: input };
  }

  return { text, number, slider, toggle, select, bareToggle, draftText, draftSelect, confirmation };
}

export type Controls = ReturnType<typeof createControls>;
