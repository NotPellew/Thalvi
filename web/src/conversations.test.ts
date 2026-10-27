import { describe, expect, it } from "vitest";
import { conversationsApi, formatAge, type Fetch } from "./conversations.ts";

function respond(status: number, body: unknown): Response {
  return new Response(body === null ? null : JSON.stringify(body), { status });
}

function recording(response: () => Response | Promise<Response>) {
  const requests: { path: string; init: RequestInit | undefined }[] = [];
  const fetchImpl: Fetch = async (path, init) => {
    requests.push({ path, init });
    return response();
  };
  return { api: conversationsApi(fetchImpl), requests };
}

describe("conversationsApi", () => {
  it("lists the rows the server sends, newest first as sent", async () => {
    const { api, requests } = recording(() =>
      respond(200, {
        sessions: [
          { id: "s2", title: "Pasta", age_s: 30 },
          { id: "s1", title: "Times tables", age_s: 4000 },
        ],
      }),
    );
    expect(await api.list()).toEqual([
      { id: "s2", title: "Pasta", ageS: 30 },
      { id: "s1", title: "Times tables", ageS: 4000 },
    ]);
    expect(requests.map((r) => r.path)).toEqual(["/sessions"]);
  });

  it("renames and deletes with the id encoded and the title as JSON", async () => {
    const { api, requests } = recording(() => respond(204, null));
    await api.rename("a b/c", "New title");
    await api.remove("a b/c");
    expect(requests.map((r) => [r.path, r.init?.method, r.init?.body])).toEqual([
      ["/sessions/a%20b%2Fc", "PATCH", '{"title":"New title"}'],
      ["/sessions/a%20b%2Fc", "DELETE", undefined],
    ]);
  });

  it.each([
    ["the server's own error message", () => respond(404, { error: "That conversation doesn't exist." }), "That conversation doesn't exist."],
    ["the status when the error body is not JSON", () => new Response("oops", { status: 502 }), "The server answered with an error (502)."],
    ["a plain message when the server cannot be reached", () => Promise.reject(new TypeError("Failed to fetch")), "Can't reach the server."],
    ["a plain message for a list this page can't read", () => respond(200, { sessions: [{ id: "s1", title: 5, age_s: 1 }] }), "The server sent a conversation list this page can't read."],
    ["a plain message for a missing list", () => respond(200, {}), "The server sent a conversation list this page can't read."],
  ])("reports %s", async (_name, response, message) => {
    const { api } = recording(response);
    await expect(api.list()).rejects.toThrow(message);
  });
});

describe("formatAge", () => {
  it.each([
    [0, "just now"],
    [59, "just now"],
    [60, "1 min ago"],
    [59 * 60 + 59, "59 min ago"],
    [3600, "1 hour ago"],
    [5 * 3600, "5 hours ago"],
    [24 * 3600, "yesterday"],
    [3 * 24 * 3600, "3 days ago"],
  ])("%i seconds is %s", (ageS, expected) => {
    expect(formatAge(ageS)).toBe(expected);
  });
});
