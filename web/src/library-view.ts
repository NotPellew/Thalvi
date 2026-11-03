import { formatAge, type ConversationSummary } from "./conversations.ts";
import { focusTarget, type Library, type LibraryState } from "./library.ts";

const LIST_ICON =
  '<svg class="o-ic" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 6h16M4 12h16M4 18h10"/></svg>';

const MARKUP = `
  <div class="o-library" hidden>
    <section class="o-lib-sheet" role="dialog" aria-modal="true" aria-labelledby="o-lib-title">
      <header class="o-lib-head">
        <h2 id="o-lib-title">Conversations</h2>
        <button class="o-lib-close" type="button">Close</button>
      </header>
      <button class="o-lib-new" type="button">New conversation</button>
      <p class="o-lib-note" role="status"></p>
      <ul class="o-lib-list"></ul>
      <section class="o-lib-preview" hidden aria-labelledby="o-lib-preview-title">
        <h3 id="o-lib-preview-title" class="o-lib-title"></h3>
        <p class="o-lib-warn"></p>
        <ol class="o-lib-turns"></ol>
        <div class="o-lib-preview-actions">
          <button class="o-lib-act o-lib-back" type="button">Back to list</button>
          <button class="o-lib-act o-lib-continue" type="button">Continue here</button>
        </div>
      </section>
    </section>
  </div>`;

function find<T extends Element>(root: ParentNode, selector: string, type: new () => T): T {
  const found = root.querySelector(selector);
  if (!(found instanceof type)) throw new Error(`${selector} is missing from the library markup`);
  return found;
}

/** A selector that finds the same control again after the list is rebuilt. */
function keyOf(element: HTMLElement): string | null {
  for (const name of ["id", "rename", "delete"]) {
    const value = element.dataset[name];
    if (value !== undefined) return `[data-${name}="${CSS.escape(value)}"]`;
  }
  return null;
}

function button(label: string, className: string, onClick: () => void): HTMLButtonElement {
  const element = document.createElement("button");
  element.type = "button";
  element.className = className;
  element.textContent = label;
  element.addEventListener("click", onClick);
  return element;
}

/** Adds the list button to the header and the list sheet to the page; the library drives both. */
export function createLibraryView(root: ParentNode, library: Library, currentId: () => string | null): void {
  const page = find(root, ".orb-app", HTMLDivElement);
  const actions = find(page, ".o-head-actions", HTMLDivElement);
  const opener = document.createElement("button");
  opener.type = "button";
  opener.className = "o-lib-open";
  opener.setAttribute("aria-label", "Conversations");
  opener.setAttribute("aria-haspopup", "dialog");
  opener.innerHTML = LIST_ICON;
  actions.prepend(opener);
  page.insertAdjacentHTML("beforeend", MARKUP);

  const overlay = find(page, ".o-library", HTMLDivElement);
  const newButton = find(page, ".o-lib-new", HTMLButtonElement);
  const note = find(page, ".o-lib-note", HTMLParagraphElement);
  const list = find(page, ".o-lib-list", HTMLUListElement);
  const preview = find(page, ".o-lib-preview", HTMLElement);
  const previewTitle = find(preview, ".o-lib-title", HTMLHeadingElement);
  const warning = find(preview, ".o-lib-warn", HTMLParagraphElement);
  const turns = find(preview, ".o-lib-turns", HTMLOListElement);
  const continueButton = find(preview, ".o-lib-continue", HTMLButtonElement);
  const background = [...page.children].filter((child) => child !== overlay);

  function row(item: ConversationSummary, state: LibraryState): HTMLLIElement {
    const line = document.createElement("li");
    line.className = "o-lib-row";
    if (state.renaming === item.id) {
      const input = document.createElement("input");
      input.className = "o-lib-input";
      input.type = "text";
      input.maxLength = 80;
      input.value = item.title;
      input.setAttribute("aria-label", "Conversation title");
      input.addEventListener("keydown", (event) => {
        if (event.key !== "Enter" || event.isComposing) return;
        event.preventDefault();
        void library.rename(item.id, input.value);
      });
      line.append(
        input,
        button("Save", "o-lib-act", () => {
          void library.rename(item.id, input.value);
        }),
        button("Cancel", "o-lib-act", () => {
          library.cancelRename();
        }),
      );
    } else if (state.confirmingDelete === item.id) {
      const ask = document.createElement("p");
      ask.className = "o-lib-ask";
      ask.textContent = `Delete “${item.title}”? This can't be undone.`;
      const cancel = button("Keep it", "o-lib-act o-lib-keep", () => {
        library.cancelDelete();
      });
      const confirm = button("Delete", "o-lib-act o-lib-danger", () => {
        void library.confirmDelete();
      });
      line.append(ask, cancel, confirm);
    } else {
      const open = button("", "o-lib-main", () => {
        void library.read(item.id);
      });
      const title = document.createElement("span");
      title.className = "o-lib-title";
      title.textContent = item.title;
      const age = document.createElement("span");
      age.className = "o-lib-age";
      age.textContent = formatAge(item.ageS);
      open.append(title, age);
      open.dataset["id"] = item.id;
      const rename = button("Rename", "o-lib-act", () => {
        library.startRename(item.id);
      });
      rename.dataset["rename"] = item.id;
      rename.setAttribute("aria-label", `Rename ${item.title}`);
      const remove = button("Delete", "o-lib-act", () => {
        library.askDelete(item.id);
      });
      remove.dataset["delete"] = item.id;
      remove.setAttribute("aria-label", `Delete ${item.title}`);
      line.append(open, rename, remove);
    }
    return line;
  }

  let previous: LibraryState = library.state;
  library.onChange((state) => {
    overlay.hidden = !state.open;
    for (const element of background) element.toggleAttribute("inert", state.open);
    opener.setAttribute("aria-expanded", String(state.open));
    note.textContent = state.error ?? (state.loading ? "Loading…" : state.items.length === 0 ? "No saved conversations yet." : "");
    const active = document.activeElement;
    const keep = active instanceof HTMLElement && list.contains(active) ? keyOf(active) : null;
    list.hidden = state.preview !== null;
    preview.hidden = state.preview === null;
    if (state.preview) {
      const here = state.preview.id === currentId();
      previewTitle.textContent = state.preview.title;
      warning.textContent = here
        ? "This conversation is open on this device."
        : state.preview.live
          ? "This conversation is open on another device. Continue here moves it to this device and stops it there."
          : "";
      warning.hidden = warning.textContent === "";
      turns.replaceChildren(
        ...state.preview.turns.map((turn) => {
          const line = document.createElement("li");
          line.className = `o-lib-turn o-lib-turn-${turn.role}`;
          const who = document.createElement("span");
          who.className = "o-who";
          who.textContent = turn.role === "user" ? "You" : "Thalvi";
          line.append(who, turn.text);
          return line;
        }),
      );
    }
    list.replaceChildren(...state.items.map((item) => row(item, state)));
    if (keep) list.querySelector<HTMLElement>(keep)?.focus();

    const target = focusTarget(previous, state);
    if (target?.control === "new") newButton.focus();
    else if (target?.control === "continue") continueButton.focus();
    else if (target?.control === "opener") opener.focus();
    else if (target?.control === "rename-input") {
      const input = list.querySelector("input");
      input?.focus();
      input?.select();
    } else if (target?.control === "keep") list.querySelector<HTMLButtonElement>(".o-lib-keep")?.focus();
    else if (target?.control === "rename") list.querySelector<HTMLButtonElement>(`[data-rename="${CSS.escape(target.id)}"]`)?.focus();
    else if (target?.control === "delete") list.querySelector<HTMLButtonElement>(`[data-delete="${CSS.escape(target.id)}"]`)?.focus();
    else if (target?.control === "row") list.querySelector<HTMLButtonElement>(`[data-id="${CSS.escape(target.id)}"]`)?.focus();
    if (state.open && !overlay.contains(document.activeElement)) newButton.focus();
    previous = state;
  });

  opener.addEventListener("click", () => {
    void library.show();
  });
  find(page, ".o-lib-close", HTMLButtonElement).addEventListener("click", () => {
    library.hide();
  });
  newButton.addEventListener("click", () => {
    library.startNew();
  });
  find(preview, ".o-lib-back", HTMLButtonElement).addEventListener("click", () => {
    library.closePreview();
  });
  continueButton.addEventListener("click", () => {
    void library.continueHere();
  });
  overlay.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    event.stopPropagation();
    if (library.state.preview !== null) library.closePreview();
    else if (library.state.renaming !== null) library.cancelRename();
    else if (library.state.confirmingDelete !== null) library.cancelDelete();
    else library.hide();
  });
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) library.hide();
  });
}
