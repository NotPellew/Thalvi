export const SPEED_STEPS = [0.75, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2] as const;

export type Speed = (typeof SPEED_STEPS)[number];

export const DEFAULT_SPEED: Speed = 1;

export const SPEED_STORAGE_KEY = "thalvi.speed";

function parseSpeed(value: unknown): Speed {
  const parsed = typeof value === "string" && value.trim() !== "" ? Number(value) : value;
  return SPEED_STEPS.find((step) => step === parsed) ?? DEFAULT_SPEED;
}

export function formatSpeed(rate: Speed): string {
  return `${String(rate)}×`;
}

export function loadSpeed(storage: Pick<Storage, "getItem">): Speed {
  try {
    return parseSpeed(storage.getItem(SPEED_STORAGE_KEY));
  } catch {
    return DEFAULT_SPEED;
  }
}

export function saveSpeed(storage: Pick<Storage, "setItem">, rate: Speed): void {
  try {
    storage.setItem(SPEED_STORAGE_KEY, String(rate));
  } catch {
    return;
  }
}
