// Picks the backend: the real one, or the fake one in `npm run dev` with `?fake`.

import type { Backend } from "./contract.ts";
import { tauriBackend } from "./tauri.ts";

export async function chooseBackend(): Promise<Backend> {
  if (import.meta.env.DEV && new URLSearchParams(location.search).has("fake")) {
    const { fakeBackend } = await import("./fake.ts");
    return fakeBackend(new URLSearchParams(location.search));
  }
  return tauriBackend();
}
// throwaway: fast-tier check
