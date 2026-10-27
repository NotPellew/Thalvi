import { describe, expect, it } from "vitest";
import type { ConversationSummary } from "./conversations.ts";
import { Library } from "./library.ts";

function setup(current: string | null = "s1") {
  const log: string[] = [];
  let rows: ConversationSummary[] = [
    { id: "s1", title: "Times tables", ageS: 30 },
    { id: "s2", title: "Pasta", ageS: 4000 },
  ];
  let failure: Error | null = null;
  const library = new Library({
    api: {
      list: () => {
        log.push("list");
        return failure ? Promise.reject(failure) : Promise.resolve(rows);
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

  it("reopens another conversation and closes, but only closes for the one already open here", async () => {
    const { library, log } = setup("s1");
    await library.show();
    library.reopen("s2");
    expect(log.slice(1)).toEqual(["open s2"]);
    expect(library.state.open).toBe(false);

    await library.show();
    library.reopen("s1");
    expect(log.slice(1)).toEqual(["open s2", "list"]);
    expect(library.state.open).toBe(false);
  });

  it("starts a new conversation and closes", async () => {
    const { library, log } = setup();
    await library.show();
    library.startNew();
    expect(log.slice(1)).toEqual(["startNew"]);
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
});
