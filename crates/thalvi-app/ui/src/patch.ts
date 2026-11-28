// JSON merge patches (RFC 7386) of the settings: what a change sends, so it carries only what it changes.

/** A merge patch: objects merge, `null` removes a key, and anything else, an array included, replaces. */
export type SettingsPatch = { [key: string]: unknown };

function isObject(value: unknown): value is { [key: string]: unknown } {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function gone(value: unknown): boolean {
  return value === undefined || value === null;
}

function same(left: unknown, right: unknown): boolean {
  if (gone(left) && gone(right)) return true;
  if (Array.isArray(left) && Array.isArray(right)) {
    return left.length === right.length && left.every((item, at) => same(item, right[at]));
  }
  if (isObject(left) && isObject(right)) {
    const keys = new Set([...Object.keys(left), ...Object.keys(right)]);
    return Array.from(keys).every((key) => same(left[key], right[key]));
  }
  return left === right;
}

/** The patch that turns `before` into `after`; empty when they are the same. */
export function settingsPatch(before: object, after: object): SettingsPatch {
  const patch: SettingsPatch = {};
  const wasValues: ReadonlyMap<string, unknown> = new Map(Object.entries(before));
  const nowValues: ReadonlyMap<string, unknown> = new Map(Object.entries(after));
  for (const key of new Set([...wasValues.keys(), ...nowValues.keys()])) {
    const was = wasValues.get(key);
    const now = nowValues.get(key);
    if (same(was, now)) continue;
    if (gone(now)) patch[key] = null;
    else if (isObject(was) && isObject(now)) patch[key] = settingsPatch(was, now);
    else patch[key] = now;
  }
  return patch;
}

