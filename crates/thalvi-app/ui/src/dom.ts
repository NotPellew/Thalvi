// Tiny DOM builder shared by both pages.

type Attrs = Record<string, string | boolean>;

/** Sets a parent's children without moving or detaching kept children already in place. */
export function setChildren(parent: HTMLElement, ...children: Node[]): void {
  const kept = new Set(children);
  for (const child of Array.from(parent.childNodes)) {
    if (!kept.has(child)) parent.removeChild(child);
  }
  children.forEach((child, index) => {
    const current = parent.childNodes[index] ?? null;
    if (current !== child) parent.insertBefore(child, current);
  });
}

/** Gives a removed item's focus to the item now at its position, else the last item, else `fallback`. */
export function focusAfterRemoval(items: readonly HTMLElement[], position: number, fallback: HTMLElement): void {
  (items[position] ?? items.at(-1) ?? fallback).focus();
}

export function el<K extends keyof HTMLElementTagNameMap>(
  tag: K,
  attrs: Attrs = {},
  ...children: (Node | string)[]
): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  for (const [name, value] of Object.entries(attrs)) {
    if (value === true) node.setAttribute(name, "");
    else if (value !== false) node.setAttribute(name, value);
  }
  node.append(...children);
  return node;
}

/** Open disclosures for the life of this window, keyed by a stable id rather than the node a rebuild replaces. */
export function disclosureMemory(): Set<string> {
  const holder = globalThis as { __thalviDisclosed?: Set<string> };
  holder.__thalviDisclosed ??= new Set<string>();
  return holder.__thalviDisclosed;
}
