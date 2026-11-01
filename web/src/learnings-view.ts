import { MAX_LEARNING_CHARS, type Learning, type Learnings, type LearningsState } from "./learnings.ts";

const MARKUP = `
  <section class="o-learn">
    <button class="o-learn-toggle" type="button" aria-expanded="false" aria-controls="o-learn-body">Learnings</button>
    <div class="o-learn-body" id="o-learn-body" hidden>
      <p class="o-lib-note" role="status"></p>
      <ul class="o-lib-list"></ul>
    </div>
  </section>`;

function find<T extends Element>(root: ParentNode, selector: string, type: new () => T): T {
  const found = root.querySelector(selector);
  if (!(found instanceof type)) throw new Error(`${selector} is missing from the learnings markup`);
  return found;
}

function button(label: string, className: string, onClick: () => void): HTMLButtonElement {
  const element = document.createElement("button");
  element.type = "button";
  element.className = className;
  element.textContent = label;
  element.addEventListener("click", onClick);
  return element;
}

function preview(text: string): string {
  return text.length > 40 ? `${text.slice(0, 40)}…` : text;
}

/** Adds the collapsible Learnings section to the library sheet; the learnings drive it. */
export function createLearningsView(root: ParentNode, learnings: Learnings): void {
  const overlay = find(root, ".o-library", HTMLDivElement);
  find(overlay, ".o-lib-sheet", HTMLElement).insertAdjacentHTML("beforeend", MARKUP);
  const toggle = find(overlay, ".o-learn-toggle", HTMLButtonElement);
  const body = find(overlay, ".o-learn-body", HTMLDivElement);
  const note = find(body, ".o-lib-note", HTMLParagraphElement);
  const list = find(body, ".o-lib-list", HTMLUListElement);

  function row(item: Learning, state: LearningsState): HTMLLIElement {
    const line = document.createElement("li");
    line.className = "o-lib-row";
    if (state.editing === item.id) {
      const input = document.createElement("textarea");
      input.className = "o-learn-input";
      input.rows = 3;
      input.maxLength = MAX_LEARNING_CHARS;
      input.value = state.draft ?? item.text;
      input.disabled = state.saving;
      input.setAttribute("aria-label", "What was learned");
      const save = button("Save", "o-lib-act", () => {
        void learnings.save(item.id, input.value);
      });
      save.disabled = state.saving;
      const cancel = button("Cancel", "o-lib-act", () => {
        learnings.cancelEdit();
      });
      cancel.disabled = state.saving;
      line.append(input, save, cancel);
    } else {
      const text = document.createElement("p");
      text.className = "o-learn-text";
      text.textContent = item.text;
      const edit = button("Edit", "o-lib-act", () => {
        learnings.startEdit(item.id);
      });
      edit.dataset["edit"] = item.id;
      edit.setAttribute("aria-label", `Edit: ${preview(item.text)}`);
      line.append(text, edit);
    }
    return line;
  }

  let previous: LearningsState = learnings.state;
  learnings.onChange((state) => {
    toggle.setAttribute("aria-expanded", String(state.open));
    body.hidden = !state.open;
    note.textContent = state.error ?? (state.loading ? "Loading…" : state.items.length === 0 ? "Nothing learned yet." : "");
    list.replaceChildren(...state.items.map((item) => row(item, state)));

    if (state.editing !== null && (state.editing !== previous.editing || (previous.saving && !state.saving))) {
      list.querySelector("textarea")?.focus();
    } else if (state.editing === null && previous.editing !== null && state.open) {
      list.querySelector<HTMLButtonElement>(`[data-edit="${CSS.escape(previous.editing)}"]`)?.focus();
    }
    previous = state;
  });

  toggle.addEventListener("click", () => {
    if (learnings.state.open) learnings.hide();
    else void learnings.show();
  });
  body.addEventListener("keydown", (event) => {
    if (event.key !== "Escape" || learnings.state.editing === null) return;
    event.stopPropagation();
    learnings.cancelEdit();
  });
  // The library closes its own sheet; fold this section too so the next visit loads fresh rows.
  new MutationObserver(() => {
    if (overlay.hidden && learnings.state.open) learnings.hide();
  }).observe(overlay, { attributes: true, attributeFilter: ["hidden"] });
}
