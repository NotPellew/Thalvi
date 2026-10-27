export interface ConversationSummary {
  readonly id: string;
  readonly title: string;
  /** Seconds since the conversation last changed, as the server counts them. */
  readonly ageS: number;
}

export interface ConversationsApi {
  list(): Promise<ConversationSummary[]>;
  rename(id: string, title: string): Promise<void>;
  remove(id: string): Promise<void>;
}

export type Fetch = (input: string, init?: RequestInit) => Promise<Response>;

const UNREACHABLE = "Can't reach the server.";

function parseList(value: unknown): ConversationSummary[] | null {
  if (typeof value !== "object" || value === null) return null;
  const rows = (value as Record<string, unknown>)["sessions"];
  if (!Array.isArray(rows)) return null;
  const list: ConversationSummary[] = [];
  for (const row of rows as unknown[]) {
    if (typeof row !== "object" || row === null) return null;
    const { id, title, age_s: ageS } = row as Record<string, unknown>;
    if (typeof id !== "string" || typeof title !== "string" || typeof ageS !== "number") return null;
    list.push({ id, title, ageS });
  }
  return list;
}

async function failure(response: Response): Promise<Error> {
  try {
    const body = (await response.json()) as unknown;
    const message = typeof body === "object" && body !== null ? (body as Record<string, unknown>)["error"] : null;
    if (typeof message === "string" && message !== "") return new Error(message);
  } catch {
    // The body was not the server's JSON error; fall through to the status.
  }
  return new Error(`The server answered with an error (${String(response.status)}).`);
}

/** Talks to the server's `/sessions` endpoints; every failure is an Error with a message fit to show. */
export function conversationsApi(fetchImpl: Fetch): ConversationsApi {
  async function call(path: string, init?: RequestInit): Promise<Response> {
    let response: Response;
    try {
      response = await fetchImpl(path, init);
    } catch {
      throw new Error(UNREACHABLE);
    }
    if (!response.ok) throw await failure(response);
    return response;
  }
  return {
    async list() {
      const response = await call("/sessions");
      const list = parseList((await response.json()) as unknown);
      if (!list) throw new Error("The server sent a conversation list this page can't read.");
      return list;
    },
    async rename(id, title) {
      await call(`/sessions/${encodeURIComponent(id)}`, {
        method: "PATCH",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ title }),
      });
    },
    async remove(id) {
      await call(`/sessions/${encodeURIComponent(id)}`, { method: "DELETE" });
    },
  };
}

/** How long ago a conversation changed, in the words the list shows. */
export function formatAge(ageS: number): string {
  const minutes = Math.floor(ageS / 60);
  if (minutes < 1) return "just now";
  if (minutes < 60) return `${String(minutes)} min ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return hours === 1 ? "1 hour ago" : `${String(hours)} hours ago`;
  const days = Math.floor(hours / 24);
  return days === 1 ? "yesterday" : `${String(days)} days ago`;
}
