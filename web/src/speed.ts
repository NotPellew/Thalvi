export const SPEED_STEPS = [0.75, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2] as const;

export const DEFAULT_SPEED = 1;

export const SPEED_STORAGE_KEY = "thalvi.speed";

export function normalizeSpeed(value: unknown): number {
  if (typeof value === "number" && (SPEED_STEPS as readonly number[]).includes(value)) return value;
  if (typeof value === "string" && value.trim() !== "") {
    const parsed = Number(value);
    if ((SPEED_STEPS as readonly number[]).includes(parsed)) return parsed;
  }
  return DEFAULT_SPEED;
}

export function formatSpeed(rate: number): string {
  return `${String(normalizeSpeed(rate))}×`;
}

export function loadSpeed(storage: Pick<Storage, "getItem">): number {
  try {
    return normalizeSpeed(storage.getItem(SPEED_STORAGE_KEY));
  } catch {
    return DEFAULT_SPEED;
  }
}

export function saveSpeed(storage: Pick<Storage, "setItem">, rate: number): void {
  try {
    storage.setItem(SPEED_STORAGE_KEY, String(normalizeSpeed(rate)));
  } catch {
    return;
  }
}
