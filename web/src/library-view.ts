import { formatAge, type ConversationSummary } from "./conversations.ts";
import type { Library, LibraryState } from "./library.ts";

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
export function createLibraryView(root: ParentNode, library: Library): void {
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
        library.reopen(item.id);
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
    list.replaceChildren(...state.items.map((item) => row(item, state)));
    if (keep) list.querySelector<HTMLElement>(keep)?.focus();

    if (state.open && !previous.open) newButton.focus();
    else if (!state.open && previous.open) opener.focus();
    else if (state.renaming !== null && state.renaming !== previous.renaming) {
      const input = list.querySelector("input");
      input?.focus();
      input?.select();
    } else if (state.confirmingDelete !== null && state.confirmingDelete !== previous.confirmingDelete) {
      list.querySelector<HTMLButtonElement>(".o-lib-keep")?.focus();
    } else if (previous.renaming !== null && state.renaming === null && state.open) {
      list.querySelector<HTMLButtonElement>(`[data-rename="${CSS.escape(previous.renaming)}"]`)?.focus();
    } else if (previous.confirmingDelete !== null && state.confirmingDelete === null && state.open) {
      list.querySelector<HTMLButtonElement>(`[data-delete="${CSS.escape(previous.confirmingDelete)}"]`)?.focus();
    }
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
  overlay.addEventListener("keydown", (event) => {
    if (event.key !== "Escape") return;
    event.stopPropagation();
    if (library.state.renaming !== null) library.cancelRename();
    else if (library.state.confirmingDelete !== null) library.cancelDelete();
    else library.hide();
  });
  overlay.addEventListener("click", (event) => {
    if (event.target === overlay) library.hide();
  });
}
