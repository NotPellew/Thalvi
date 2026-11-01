import { describe, expect, it } from "vitest";
import type { Fetch } from "./conversations.ts";
import { Learnings, learningsApi, type Learning } from "./learnings.ts";

function respond(status: number, body: unknown): Response {
  return new Response(body === null ? null : JSON.stringify(body), { status });
}

function recording(response: () => Response | Promise<Response>) {
  const requests: { path: string; init: RequestInit | undefined }[] = [];
  const fetchImpl: Fetch = async (path, init) => {
    requests.push({ path, init });
    return response();
  };
  return { api: learningsApi(fetchImpl), requests };
}

describe("learningsApi", () => {
  it("lists the rows the server sends, with a null source for ones the user wrote", async () => {
    const { api, requests } = recording(() =>
      respond(200, {
        learnings: [
          { id: "l1", text: "Likes tea", source_session: "s1", updated_ms: 5 },
          { id: "l2", text: "Has two cats", source_session: null, updated_ms: 6 },
        ],
      }),
    );
    expect(await api.list()).toEqual([
      { id: "l1", text: "Likes tea", sourceSession: "s1" },
      { id: "l2", text: "Has two cats", sourceSession: null },
    ]);
    expect(requests.map((r) => r.path)).toEqual(["/learnings"]);
  });

  it("edits with the id encoded and the text as JSON", async () => {
    const { api, requests } = recording(() => respond(204, null));
    await api.edit("a b/c", "Likes coffee");
    expect(requests.map((r) => [r.path, r.init?.method, r.init?.body])).toEqual([
      ["/learnings/a%20b%2Fc", "PATCH", '{"text":"Likes coffee"}'],
    ]);
  });

  it.each([
    ["the server's own error message", () => respond(500, { error: "Could not save the learning: disk full" }), "Could not save the learning: disk full"],
    ["the status when the error body is not JSON", () => new Response("oops", { status: 502 }), "The server answered with an error (502)."],
    ["a plain message when the server cannot be reached", () => Promise.reject(new TypeError("Failed to fetch")), "Can't reach the server."],
    ["a plain message for a row this page can't read", () => respond(200, { learnings: [{ id: "l1", text: 5, source_session: null }] }), "The server sent a list of learnings this page can't read."],
    ["a plain message for a source of the wrong type", () => respond(200, { learnings: [{ id: "l1", text: "x", source_session: 7 }] }), "The server sent a list of learnings this page can't read."],
    ["a plain message for a missing list", () => respond(200, {}), "The server sent a list of learnings this page can't read."],
  ])("reports %s", async (_name, response, message) => {
    const { api } = recording(response);
    await expect(api.list()).rejects.toThrow(message);
  });
});

function setup() {
  const log: string[] = [];
  let rows: Learning[] = [
    { id: "l1", text: "Likes tea", sourceSession: "s1" },
    { id: "l2", text: "Has two cats", sourceSession: "s1" },
  ];
  let failure: Error | null = null;
  const learnings = new Learnings({
    list: () => {
      log.push("list");
      return failure ? Promise.reject(failure) : Promise.resolve(rows);
    },
    edit: (id, text) => {
      log.push(`edit ${id} ${text}`);
      if (failure) return Promise.reject(failure);
      rows = rows.map((row) => (row.id === id ? { ...row, text } : row));
      return Promise.resolve();
    },
  });
  return {
    learnings,
    log,
    failWith: (message: string | null) => {
      failure = message === null ? null : new Error(message);
    },
  };
}

describe("Learnings", () => {
  it.each([false, true])("ignores an old load after the panel is reopened (failure: %s)", async (fails) => {
    let resolve!: (items: Learning[]) => void;
    let reject!: (error: Error) => void;
    const pending = new Promise<Learning[]>((done, fail) => { resolve = done; reject = fail; });
    const fresh: Learning[] = [{ id: "l1", text: "Fresh text", sourceSession: null }];
    let calls = 0;
    const learnings = new Learnings({
      list: () => ++calls === 1 ? pending : Promise.resolve(fresh),
      edit: () => Promise.resolve(),
    });
    const old = learnings.show();
    learnings.hide();
    await learnings.show();
    learnings.startEdit("l1");
    if (fails) reject(new Error("Old request failed"));
    else resolve([{ id: "l1", text: "Old text", sourceSession: null }]);
    await old;
    expect(learnings.state).toMatchObject({ items: fresh, editing: "l1", error: null });
  });

  it.each([false, true])("ignores an old save after the editor is reopened (failure: %s)", async (fails) => {
    let resolve!: () => void;
    let reject!: (error: Error) => void;
    const pending = new Promise<void>((done, fail) => { resolve = done; reject = fail; });
    const fresh: Learning[] = [{ id: "l1", text: "Fresh text", sourceSession: null }];
    const learnings = new Learnings({ list: () => Promise.resolve(fresh), edit: () => pending });
    await learnings.show();
    learnings.startEdit("l1");
    const old = learnings.save("l1", "Old save");
    learnings.hide();
    await learnings.show();
    learnings.startEdit("l1");
    await learnings.save("l1", " ");
    const current = learnings.state;
    if (fails) reject(new Error("Old save failed"));
    else resolve();
    await old;
    expect(learnings.state).toBe(current);
  });

  it("starts closed and loads fresh rows each time the user opens it", async () => {
    const { learnings, log } = setup();
    expect(learnings.state.open).toBe(false);
    expect(log).toEqual([]);

    const shown = learnings.show();
    expect(learnings.state).toMatchObject({ open: true, loading: true, items: [] });
    await shown;
    expect(learnings.state.loading).toBe(false);
    expect(learnings.state.items.map((item) => item.text)).toEqual(["Likes tea", "Has two cats"]);

    learnings.hide();
    expect(learnings.state).toMatchObject({ open: false, items: [] });
    await learnings.show();
    expect(log).toEqual(["list", "list"]);
  });

  it("says in plain words when the rows cannot be loaded", async () => {
    const { learnings, failWith } = setup();
    failWith("Can't reach the server.");
    await learnings.show();
    expect(learnings.state).toMatchObject({ open: true, loading: false, items: [], error: "Can't reach the server." });
  });

  it("saves the trimmed edit, shows it in the row and leaves edit mode", async () => {
    const { learnings, log } = setup();
    await learnings.show();
    learnings.startEdit("l1");
    expect(learnings.state.editing).toBe("l1");

    await learnings.save("l1", "  Likes coffee ");
    expect(log.slice(1)).toEqual(["edit l1 Likes coffee"]);
    expect(learnings.state).toMatchObject({ editing: null, draft: null, saving: false, error: null });
    expect(learnings.state.items.map((item) => item.text)).toEqual(["Likes coffee", "Has two cats"]);
  });

  it("keeps the edit open with the typed text when saving fails, and a retry can succeed", async () => {
    const { learnings, log, failWith } = setup();
    await learnings.show();
    learnings.startEdit("l1");
    failWith("Could not save the learning: disk full");

    await learnings.save("l1", "Likes coffee");
    expect(learnings.state).toMatchObject({
      editing: "l1",
      draft: "Likes coffee",
      saving: false,
      error: "Could not save the learning: disk full",
    });
    expect(learnings.state.items[0]?.text).toBe("Likes tea");

    failWith(null);
    await learnings.save("l1", "Likes coffee");
    expect(log.filter((entry) => entry.startsWith("edit"))).toHaveLength(2);
    expect(learnings.state).toMatchObject({ editing: null, error: null });
    expect(learnings.state.items[0]?.text).toBe("Likes coffee");
  });

  it("refuses a blank edit without asking the server and keeps what was typed", async () => {
    const { learnings, log } = setup();
    await learnings.show();
    learnings.startEdit("l1");
    await learnings.save("l1", "   ");
    expect(learnings.state).toMatchObject({ editing: "l1", draft: "   ", error: "The text can't be empty." });
    expect(log).toEqual(["list"]);
  });

  it("cancels an edit and drops the draft and any error", async () => {
    const { learnings, failWith } = setup();
    await learnings.show();
    learnings.startEdit("l1");
    failWith("nope");
    await learnings.save("l1", "Changed");
    learnings.cancelEdit();
    expect(learnings.state).toMatchObject({ editing: null, draft: null, error: null });
    expect(learnings.state.items[0]?.text).toBe("Likes tea");
  });

  it("ignores a second save and any edit changes while one is in flight", async () => {
    const log: string[] = [];
    let finish: () => void = () => undefined;
    const learnings = new Learnings({
      list: () => Promise.resolve([{ id: "l1", text: "Likes tea", sourceSession: null }]),
      edit: (id, text) => {
        log.push(`edit ${id} ${text}`);
        return new Promise<void>((resolve) => {
          finish = resolve;
        });
      },
    });
    await learnings.show();
    learnings.startEdit("l1");
    const first = learnings.save("l1", "One");
    expect(learnings.state.saving).toBe(true);
    await learnings.save("l1", "Two");
    learnings.cancelEdit();
    expect(learnings.state.editing).toBe("l1");
    finish();
    await first;
    expect(log).toEqual(["edit l1 One"]);
    expect(learnings.state).toMatchObject({ editing: null, saving: false });
  });
});
