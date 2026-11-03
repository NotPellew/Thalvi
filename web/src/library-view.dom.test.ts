// @vitest-environment jsdom

import { afterEach, expect, it, vi } from "vitest";
import type { ConversationDetail } from "./conversations.ts";
import { Library } from "./library.ts";
import { createLibraryView } from "./library-view.ts";

afterEach(() => {
  document.body.replaceChildren();
});

function mount(detail: ConversationDetail, currentId: string | null) {
  const root = document.createElement("div");
  root.innerHTML = '<div class="orb-app"><div class="o-head-actions"></div></div>';
  document.body.append(root);
  const open = vi.fn();
  const library = new Library({
    api: {
      list: () => Promise.resolve([{ id: detail.id, title: detail.title, ageS: 5 }]),
      detail: () => Promise.resolve(detail),
      rename: () => Promise.resolve(),
      remove: () => Promise.resolve(),
    },
    currentId: () => currentId,
    open,
    startNew: () => undefined,
  });
  createLibraryView(root, library, () => currentId);
  const query = (selector: string): HTMLElement => {
    const found = root.querySelector<HTMLElement>(selector);
    if (!found) throw new Error(`${selector} is missing`);
    return found;
  };
  return { library, open, query };
}

it.each([
  ["moving it from another device", "s1", true, /moves it to this device and stops it there/],
  ["the one already open here", "s2", true, /open on this device/],
  ["an idle one", "s1", false, null],
])("previews %s with the right notice, and joins only when Continue here is pressed", async (_name, current, live, notice) => {
  const { library, open, query } = mount(
    { id: "s2", title: "Pasta", turns: [{ role: "user", text: "Boil water" }, { role: "assistant", text: "Done." }], live },
    current === "s2" ? "s2" : "s1",
  );
  await library.show();
  query(".o-lib-main").click();
  await vi.waitFor(() => {
    expect(query(".o-lib-preview").hidden).toBe(false);
  });

  expect(query(".o-lib-list").hidden).toBe(true);
  expect([...document.querySelectorAll(".o-lib-turn")].map((turn) => turn.textContent)).toEqual(["Boil water", "Done."]);
  const warning = query(".o-lib-warn");
  if (notice) expect(warning.textContent).toMatch(notice);
  else expect(warning.hidden).toBe(true);
  expect(open).not.toHaveBeenCalled();

  query(".o-lib-continue").click();
  await vi.waitFor(() => {
    expect(query(".o-library").hidden).toBe(true);
  });
  expect(open).toHaveBeenCalledTimes(current === "s2" ? 0 : 1);
});
