// The Customize section: controls generated from the look descriptors, and a live preview.

import { errorText, OVERLAY_PREVIEW_STATES } from "./contract.ts";
import type { Backend, JsonObject, Look, LookProperty, LookScheme, LookStyles, LookValue, Settings } from "./contract.ts";
import type { Field } from "./controls.ts";
import { el } from "./dom.ts";
import { LOOKS } from "./look.ts";
import { row, type PageContext } from "./settings-pages.ts";

const SECTIONS: readonly (readonly [string, readonly string[]])[] = [
  ["Colors", ["colors"]],
  ["Typography", ["font"]],
  ["Surfaces", ["spacing", "shape", "border", "surface"]],
  ["Effects", ["glow", "decoration", "motion"]],
];

type ReadonlyJson = string | number | boolean | null | readonly ReadonlyJson[] | { readonly [key: string]: ReadonlyJson };
type ReadonlyJsonObject = { readonly [key: string]: ReadonlyJson };

function isJsonObject(value: ReadonlyJson | undefined): value is ReadonlyJsonObject {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function filePath(id: string, scheme: LookScheme): string[] {
  const parts = id.split(".");
  return parts[0] === "colors" ? ["colors", scheme, ...parts.slice(1)] : parts;
}

function objectAt(root: JsonObject | undefined, path: readonly string[]): JsonObject | undefined;
function objectAt(root: ReadonlyJsonObject | undefined, path: readonly string[]): ReadonlyJsonObject | undefined;
function objectAt(root: ReadonlyJsonObject | undefined, path: readonly string[]): ReadonlyJsonObject | undefined {
  let node: ReadonlyJsonObject | undefined = root;
  for (const key of path) {
    if (node === undefined) return undefined;
    const next = node[key];
    node = isJsonObject(next) ? next : undefined;
  }
  return node;
}

function has(root: ReadonlyJsonObject | undefined, path: readonly string[]): boolean {
  const parent = objectAt(root, path.slice(0, -1));
  const leaf = path.at(-1);
  return parent !== undefined && leaf !== undefined && parent[leaf] !== undefined;
}

function setLeaf(root: JsonObject, path: readonly string[], value: LookValue): void {
  let node = root;
  for (const key of path.slice(0, -1)) {
    const existing = node[key];
    if (typeof existing === "object" && existing !== null && !Array.isArray(existing)) {
      node = existing;
    } else {
      const fresh: JsonObject = {};
      node[key] = fresh;
      node = fresh;
    }
  }
  const leaf = path.at(-1);
  if (leaf !== undefined) node[leaf] = value;
}

function deleteLeaf(root: JsonObject | undefined, path: readonly string[]): void {
  const parent = objectAt(root, path.slice(0, -1));
  const leaf = path.at(-1);
  if (parent !== undefined && leaf !== undefined) Reflect.deleteProperty(parent, leaf);
}

function say(node: HTMLElement, message: string): void {
  node.textContent = message;
  node.hidden = message === "";
}

function button(label: string, attr: string, onClick: () => void): HTMLButtonElement {
  const node = el("button", { type: "button", class: "quiet", [attr]: "" }, label);
  node.addEventListener("click", onClick);
  return node;
}

interface Preview {
  node: HTMLElement;
  draw(): Promise<void>;
}

/** Inert sample controls in the window's look, and the real overlay for each state. */
function overlayPreview(backend: Backend): Preview {
  const sample = el("div", { class: "preview-sample", "aria-hidden": "true", inert: true },
    el("input", { type: "checkbox", role: "switch", "aria-label": "Sample switch" }),
    el("select", { "aria-label": "Sample choice" }, el("option", {}, "Choose one")),
    el("button", { type: "button", class: "primary" }, "Primary"),
    el("span", { class: "badge" }, "Badge"),
    el("p", { class: "note" }, "Something needs your attention."),
  );
  const strip = el("div", { class: "preview-strip" });
  const canvases = OVERLAY_PREVIEW_STATES.map((state) => {
    const canvas = el("canvas", { class: "preview-canvas", "data-preview-state": state });
    strip.append(el("div", { class: "preview-cell" }, el("span", { class: "preview-cap" }, state), canvas));
    return canvas;
  });
  const note = el("p", { class: "note", role: "alert", hidden: true });
  const node = el("div", { class: "preview wide", "data-cust-preview": "" }, sample, strip, note);
  let generation = 0;

  async function draw(): Promise<void> {
    const request = ++generation;
    const scale = window.devicePixelRatio;
    try {
      const buffers = await Promise.all(OVERLAY_PREVIEW_STATES.map((state) => backend.previewOverlay(state, scale)));
      if (request !== generation) return;
      say(note, "");
      buffers.forEach((buffer, index) => {
        const canvas = canvases[index];
        if (canvas === undefined) return;
        const view = new DataView(buffer);
        const width = view.getUint32(0, true);
        const height = view.getUint32(4, true);
        canvas.width = width;
        canvas.height = height;
        canvas.style.width = `${String(width / scale)}px`;
        const context = canvas.getContext("2d");
        if (context === null) throw new Error("This window cannot draw the preview.");
        context.putImageData(new ImageData(new Uint8ClampedArray(buffer, 8), width, height), 0, 0);
      });
    } catch (error) {
      if (request !== generation) return;
      for (const canvas of canvases) {
        canvas.width = 0;
        canvas.height = 0;
      }
      say(note, errorText(error));
    }
  }

  return { node, draw };
}

interface FieldRow {
  id: string;
  path: string[];
  field: HTMLElement;
  swatch: HTMLElement | null;
  reset: HTMLButtonElement;
  problem: HTMLElement;
  shown: LookValue;
  last: LookValue;
}

/** The Customize section for the look saved in Settings, plus its preview and banners. */
export function customize(context: PageContext): HTMLElement[] {
  const { settings, controls, backend, apply, live } = context;
  let look: Look = settings.look;
  let styles: LookStyles | null = context.lookStyles;
  let rows: FieldRow[] = [];
  let builtLook: Look | null = null;
  let bannerKey = "";
  const banner = el("div", { class: "cust-banner wide", "data-cust-banner": "" });
  // A group with no visible row is hidden, so the banner and the preview are rows of their own.
  const bannerRow = row("Custom styling", null, banner);
  bannerRow.classList.add("off");
  const fields = el("div", { class: "cust-fields", "data-cust-fields": "" });
  const actions = el("div", { class: "cust-actions", "data-cust-actions": "" });
  const preview = overlayPreview(backend);
  const holder = el("div", { class: "cust", "data-customize": "" }, fields, actions);
  let seen = false;
  const before = live.opened;
  live.opened = (id) => {
    before(id);
    if (id === "overlay") {
      seen = true;
      void preview.draw();
    }
  };

  const title = (): string => LOOKS.find(([name]) => name === look)?.[1] ?? look;
  const changes = (): ReadonlyJsonObject | undefined => settings.themes?.[look];

  function showValue(entry: FieldRow, value: LookValue): void {
    entry.shown = value;
    if (entry.field instanceof HTMLInputElement) {
      if (entry.field.type === "checkbox") entry.field.checked = value === true;
      else entry.field.value = String(value);
    }
    if (entry.swatch !== null && typeof value === "string") entry.swatch.style.backgroundColor = value;
  }

  function refreshResets(): void {
    for (const entry of rows) {
      const disabled = !has(changes(), entry.path);
      if (entry.reset.disabled === disabled) continue;
      entry.reset.disabled = disabled;
      if (disabled && document.activeElement === entry.reset) entry.field.focus();
    }
  }

  function syncRows(): void {
    const values = styles?.values[look];
    for (const entry of rows) {
      const value = values?.[entry.id];
      if (value !== undefined && value !== entry.shown) {
        showValue(entry, value);
        entry.last = value;
      }
    }
    refreshResets();
  }

  function afterChange(): void {
    refreshResets();
    if (seen) void preview.draw();
  }

  function set(path: string[], value: LookValue): Promise<void> {
    const target = look;
    return apply((draft) => {
      draft.themes ??= { version: 1 };
      const entry = draft.themes[target] ?? {};
      draft.themes[target] = entry;
      if (value === "") deleteLeaf(entry, path);
      else setLeaf(entry, path, value);
    }).then(afterChange);
  }

  function reset(entry: FieldRow): void {
    say(entry.problem, "");
    const target = look;
    void apply((draft) => {
      deleteLeaf(draft.themes?.[target], entry.path);
    }).then(afterChange, (error: unknown) => { say(entry.problem, errorText(error)); });
  }

  function control(property: LookProperty): HTMLElement | null {
    if (styles === null) return null;
    const value = styles.values[look][property.id];
    if (value === undefined) return null;
    const path = filePath(property.id, styles.schemes[look]);
    let field: Field;
    const commit = (wanted: LookValue): Promise<void> =>
      set(path, wanted).then(
        () => {
          entry.last = wanted;
          entry.shown = wanted;
          if (entry.swatch !== null && typeof wanted === "string") entry.swatch.style.backgroundColor = wanted;
        },
        (error: unknown) => {
          showValue(entry, entry.last);
          throw error;
        },
      );

    let swatch: HTMLElement | null = null;
    if (property.unit === "color" || property.unit === "text") {
      if (typeof value !== "string") return null;
      field = controls.text(value, commit, property.unit === "text" ? { placeholder: "System font" } : {});
      if (property.unit === "color") {
        swatch = el("span", { class: "swatch", "data-swatch": "" });
        swatch.style.backgroundColor = value;
      }
    } else if (property.unit === "number") {
      if (typeof value !== "number" || property.min === null || property.max === null || property.step === null) return null;
      field = controls.slider(value, { min: property.min, max: property.max, step: property.step }, commit);
    } else {
      if (typeof value !== "boolean") return null;
      field = controls.toggle(value, commit);
    }

    const controlNode = swatch === null ? field.node : el("span", { class: "swatch-field" }, swatch, field.node);
    const problem = el("p", { class: "note", role: "alert", hidden: true });
    const resetButton = button("↺", "data-reset", () => { reset(entry); });
    resetButton.setAttribute("data-reset", property.id);
    resetButton.setAttribute("aria-label", `Reset ${property.label}`);
    resetButton.disabled = !has(changes(), path);
    const entry: FieldRow = { id: property.id, path, field: field.field, swatch, reset: resetButton, problem, shown: value, last: value };
    rows.push(entry);
    return row(property.label, null, { node: el("span", { class: "cust-control" }, controlNode, resetButton, problem), field: field.field });
  }

  function renderFields(): void {
    rows = [];
    const properties = styles?.properties ?? [];
    const sections = SECTIONS.flatMap(([heading, categories]) => {
      const rowNodes = properties
        .filter((property) => categories.includes(property.category) && property.looks.includes(look))
        .map(control)
        .filter((node): node is HTMLElement => node !== null);
      return rowNodes.length === 0 ? [] : [el("section", { class: "cust-cat", "data-category": heading }, el("h5", {}, heading), ...rowNodes)];
    });
    fields.replaceChildren(...sections);
    builtLook = look;
  }

  function act(edit: (draft: Settings) => void, problem: HTMLElement): void {
    say(problem, "");
    void apply(edit).then(
      () => { render(); },
      (error: unknown) => { say(problem, errorText(error)); },
    );
  }

  function renderBanner(suspended: boolean, reason: string | null): boolean {
    const key = suspended ? `s:${title()}` : reason !== null ? `u:${title()}:${reason}` : "";
    if (key === bannerKey) return key !== "";
    bannerKey = key;
    if (key === "") {
      bannerRow.classList.add("off");
      banner.replaceChildren();
      return false;
    }
    const problem = el("p", { class: "note", role: "alert", hidden: true });
    const parts: HTMLElement[] = [
      el("span", {}, suspended
        ? `Custom styling is off for ${title()} — `
        : `Your ${title()} customizations could not be read (${String(reason)}). `),
    ];
    if (suspended) {
      parts.push(button("Turn back on", "data-banner-on", () => { act((draft) => {
        const current = draft.themes?.[look];
        if (current !== undefined) Reflect.deleteProperty(current, "suspended");
      }, problem); }));
    }
    parts.push(button(`Reset ${title()}`, "data-banner-reset", () => { act((draft) => {
      if (draft.themes !== undefined) Reflect.deleteProperty(draft.themes, look);
    }, problem); }));
    parts.push(problem);
    banner.replaceChildren(...parts);
    bannerRow.classList.remove("off");
    return true;
  }

  function renderActions(): void {
    const problem = el("p", { class: "note", role: "alert", hidden: true });
    const resetLook = button("Reset this look", "data-reset-look", () => { act((draft) => {
      if (draft.themes !== undefined) Reflect.deleteProperty(draft.themes, look);
    }, problem); });
    actions.replaceChildren(resetLook, problem);
  }

  function render(): void {
    const entry = changes();
    const suspended = entry?.suspended === true;
    const unreadable = entry?.unreadable;
    const reason = typeof unreadable === "string" ? unreadable : null;
    const showing = renderBanner(suspended, reason);
    fields.hidden = showing;
    actions.hidden = showing;
    if (showing) {
      builtLook = null;
    } else if (builtLook !== look) {
      renderFields();
      renderActions();
    } else {
      syncRows();
    }
    if (seen) void preview.draw();
  }

  live.look = (next: Look) => {
    look = next;
    render();
  };
  live.lookStyles = (next: LookStyles) => {
    if (styles?.schemes[look] !== next.schemes[look]) builtLook = null;
    styles = next;
    render();
  };
  render();

  return [el("h4", { "data-group": "" }, "Customize"), bannerRow, holder, row("Preview", null, preview.node)];
}
