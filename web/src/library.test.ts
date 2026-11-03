import { describe, expect, it } from "vitest";
import type { ConversationDetail, ConversationSummary } from "./conversations.ts";
import { focusTarget, Library, type FocusTarget, type LibraryState } from "./library.ts";

function setup(current: string | null = "s1") {
  const log: string[] = [];
  let rows: ConversationSummary[] = [
    { id: "s1", title: "Times tables", ageS: 30 },
    { id: "s2", title: "Pasta", ageS: 4000 },
  ];
  let failure: Error | null = null;
  const live = new Set<string>();
  const missing = new Set<string>();
  const library = new Library({
    api: {
      list: () => {
        log.push("list");
        return failure ? Promise.reject(failure) : Promise.resolve(rows);
      },
      detail: (id) => {
        log.push(`detail ${id}`);
        const row = rows.find((candidate) => candidate.id === id);
        if (missing.has(id) || !row) return Promise.reject(new Error("That conversation doesn't exist."));
        const detail: ConversationDetail = {
          id,
          title: row.title,
          turns: [{ role: "user", text: `hello from ${id}` }],
          live: live.has(id),
        };
        return Promise.resolve(detail);
      },
      rename: (id, title) => {
        log.push(`rename ${id} ${title}`);
        rows = rows.map((row) => (row.id === id ? { ...row, title } : row));
        return Promise.resolve();
      },
      remove: (id) => {
        log.push(`remove ${id}`);
        if (failure) return Promise.reject(failure);
        rows = rows.filter((row) => row.id !== id);
        return Promise.resolve();
      },
    },
    currentId: () => current,
    open: (id) => log.push(`open ${id}`),
    startNew: () => log.push("startNew"),
  });
  return {
    library,
    log,
    setLive: (id: string) => live.add(id),
    deleteBehindTheBack: (id: string) => missing.add(id),
    failWith: (message: string) => {
      failure = new Error(message);
    },
  };
}

describe("Library", () => {
  it("opens only when asked, then lists the saved conversations", async () => {
    const { library, log } = setup();
    expect(library.state.open).toBe(false);
    expect(log).toEqual([]);

    const shown = library.show();
    expect(library.state).toMatchObject({ open: true, loading: true, items: [] });
    await shown;
    expect(library.state.loading).toBe(false);
    expect(library.state.items.map((item) => item.title)).toEqual(["Times tables", "Pasta"]);

    library.hide();
    expect(library.state.open).toBe(false);
  });

  it("reads a conversation for a preview without joining it, then goes back to the list", async () => {
    const { library, log } = setup("s1");
    await library.show();
    await library.read("s2");
    expect(log).toEqual(["list", "detail s2"]);
    expect(library.state.preview).toEqual({
      id: "s2",
      title: "Pasta",
      turns: [{ role: "user", text: "hello from s2" }],
      live: false,
    });
    expect(library.state.open).toBe(true);

    library.closePreview();
    expect(library.state.preview).toBeNull();
    expect(log).toEqual(["list", "detail s2"]);
  });

  it.each([
    ["an idle conversation", false, false],
    ["a conversation the user is reading while another device uses it", true, true],
  ])("Continue here joins %s only after re-checking it", async (_name, isLive) => {
    const { library, log, setLive } = setup("s1");
    if (isLive) setLive("s2");
    await library.show();
    await library.read("s2");
    expect(library.state.preview?.live).toBe(isLive);
    expect(log).not.toContain("open s2");

    await library.continueHere();
    expect(log.slice(1)).toEqual(["detail s2", "detail s2", "open s2"]);
    expect(library.state.open).toBe(false);
  });

  it("Continue here on the conversation already open here just closes the list", async () => {
    const { library, log } = setup("s1");
    await library.show();
    await library.read("s1");
    await library.continueHere();
    expect(log.slice(1)).toEqual(["detail s1"]);
    expect(library.state.open).toBe(false);
  });

  it("shows a plain error and joins nothing when the conversation was deleted during the preview", async () => {
    const { library, log, deleteBehindTheBack } = setup("s1");
    await library.show();
    await library.read("s2");
    deleteBehindTheBack("s2");

    await library.continueHere();
    expect(log.filter((entry) => entry.startsWith("open") || entry === "startNew")).toEqual([]);
    expect(library.state).toMatchObject({ open: true, preview: null, error: "That conversation doesn't exist." });
  });

  it.each([
    ["a second Continue here", (library: Library) => library.continueHere(), ["open s2"]],
    [
      "New conversation",
      (library: Library) => {
        library.startNew();
        return Promise.resolve();
      },
      ["startNew"],
    ],
    [
      "Back to list",
      (library: Library) => {
        library.closePreview();
        return Promise.resolve();
      },
      [],
    ],
  ])("joins at most once and not at all after %s while the check runs", async (_name, act, joined) => {
    const { library, log } = setup("s1");
    await library.show();
    await library.read("s2");
    const first = library.continueHere();
    await act(library);
    await first;
    expect(log.filter((entry) => entry.startsWith("open") || entry === "startNew")).toEqual(joined);
  });

  it("asks again, with the warning, when another device took the conversation while it was being read", async () => {
    const { library, log, setLive } = setup("s1");
    await library.show();
    await library.read("s2");
    setLive("s2");

    await library.continueHere();
    expect(log).not.toContain("open s2");
    expect(library.state.preview?.live).toBe(true);
    expect(library.state.error).toMatch(/Another device started using this conversation/);

    await library.continueHere();
    expect(log).toContain("open s2");
  });

  it("shows the server's message and stays in the list when a conversation cannot be read", async () => {
    const { library, log, deleteBehindTheBack } = setup("s1");
    await library.show();
    deleteBehindTheBack("s2");
    await library.read("s2");
    expect(library.state).toMatchObject({ preview: null, loading: false, error: "That conversation doesn't exist." });
    expect(log).not.toContain("open s2");
  });

  it("starts a new conversation and closes", async () => {
    const { library, log } = setup();
    await library.show();
    library.startNew();
    expect(log.slice(1)).toEqual(["startNew"]);
    expect(log.some((entry) => entry.startsWith("open") || entry.startsWith("detail"))).toBe(false);
    expect(library.state.open).toBe(false);
  });

  it("deletes only after the user confirms, and keeps the conversation if they decline", async () => {
    const { library, log } = setup("s1");
    await library.show();
    library.askDelete("s2");
    expect(library.state.confirmingDelete).toBe("s2");
    expect(log).toEqual(["list"]);

    library.cancelDelete();
    await library.confirmDelete();
    expect(log).toEqual(["list"]);

    library.askDelete("s2");
    await library.confirmDelete();
    expect(log).toEqual(["list", "remove s2", "list"]);
    expect(library.state.items.map((item) => item.id)).toEqual(["s1"]);
    expect(library.state.confirmingDelete).toBeNull();
  });

  it("starts a new conversation when the one open here is deleted", async () => {
    const { library, log } = setup("s1");
    await library.show();
    library.askDelete("s1");
    await library.confirmDelete();
    expect(log).toEqual(["list", "remove s1", "startNew", "list"]);
  });

  it("renames with a trimmed title and a blank one just leaves rename mode", async () => {
    const { library, log } = setup();
    await library.show();
    library.startRename("s2");
    expect(library.state.renaming).toBe("s2");
    await library.rename("s2", "   ");
    expect(log).toEqual(["list"]);
    expect(library.state.renaming).toBeNull();

    await library.rename("s2", "  Dinner plan ");
    expect(log.slice(1)).toEqual(["rename s2 Dinner plan", "list"]);
    expect(library.state.renaming).toBeNull();
    expect(library.state.items.find((item) => item.id === "s2")?.title).toBe("Dinner plan");
  });

  it("tells the user in plain words when the server fails, and keeps the conversation open here", async () => {
    const { library, log, failWith } = setup("s1");
    await library.show();
    failWith("Can't reach the server.");
    library.askDelete("s1");
    await library.confirmDelete();
    expect(library.state.error).toBe("Can't reach the server.");
    expect(log).not.toContain("startNew");
    expect(library.state.items).toHaveLength(2);
  });

  it("keeps the rename and delete prompts up until the list is refreshed, so the view renders the final rows once", async () => {
    const { library } = setup("s1");
    await library.show();
    const seen: LibraryState[] = [];
    library.onChange((state) => seen.push(state));

    library.startRename("s2");
    await library.rename("s2", "Dinner plan");
    library.askDelete("s2");
    await library.confirmDelete();

    expect(seen.map((state) => [state.renaming, state.confirmingDelete, state.items.map((item) => item.title)])).toEqual([
      ["s2", null, ["Times tables", "Pasta"]],
      [null, null, ["Times tables", "Dinner plan"]],
      [null, "s2", ["Times tables", "Dinner plan"]],
      [null, null, ["Times tables"]],
    ]);
  });
});

describe("focusTarget", () => {
  const item = (id: string) => ({ id, title: id, ageS: 1 });
  const state = (patch: Partial<LibraryState>): LibraryState => ({
    open: true,
    loading: false,
    items: [item("a"), item("b"), item("c")],
    preview: null,
    renaming: null,
    confirmingDelete: null,
    error: null,
    ...patch,
  });
  const cases: [string, LibraryState, LibraryState, FocusTarget | null][] = [
    ["opening the list focuses New conversation", state({ open: false }), state({}), { control: "new" }],
    ["closing the list returns to its button", state({}), state({ open: false }), { control: "opener" }],
    ["opening a preview focuses Continue here", state({}), state({ preview: { id: "b", title: "b", turns: [], live: false } }), { control: "continue" }],
    ["leaving a preview returns to that row", state({ preview: { id: "b", title: "b", turns: [], live: false } }), state({}), { control: "row", id: "b" }],
    ["starting a rename focuses the title input", state({}), state({ renaming: "b" }), { control: "rename-input" }],
    ["asking to delete focuses Keep it", state({}), state({ confirmingDelete: "b" }), { control: "keep" }],
    ["a saved rename returns to that row's Rename button", state({ renaming: "b" }), state({}), { control: "rename", id: "b" }],
    [
      "a confirmed delete moves to the next row",
      state({ confirmingDelete: "b" }),
      state({ items: [item("a"), item("c")] }),
      { control: "row", id: "c" },
    ],
    [
      "deleting the last row moves to New conversation",
      state({ confirmingDelete: "c" }),
      state({ items: [item("a"), item("b")] }),
      { control: "new" },
    ],
    ["a delete that failed returns to that row's Delete button", state({ confirmingDelete: "b" }), state({}), { control: "delete", id: "b" }],
    ["an unrelated refresh leaves focus alone", state({}), state({ error: "No." }), null],
  ];
  it.each(cases)("%s", (_name, previous, next, expected) => {
    expect(focusTarget(previous, next)).toEqual(expected);
  });
});
