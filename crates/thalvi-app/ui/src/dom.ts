// Tiny DOM builder shared by both pages.

type Attrs = Record<string, string | boolean>;

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
