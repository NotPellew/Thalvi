// Geometry checks shared by the CI sweep and the manual Settings verifier.

interface Box {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface MeasuredBox extends Box {
  label: string;
  visible: boolean;
  unoccluded: boolean;
  problems: string[];
}

interface LayoutReport {
  boxes: MeasuredBox[];
  overflow: string[];
}

export function measureSettingsBoxes(elements: Element[], onlyVisible = false): LayoutReport {
  const tolerance = 0.5;
  const visible = (element: Element): boolean => {
    const closed = element.closest("details:not([open])");
    return element.closest("[inert]") === null && element.checkVisibility({ visibilityProperty: true }) &&
      (closed === null || closed.querySelector(":scope > summary")?.contains(element) === true);
  };
  const labelsOf = (element: Element) => element instanceof HTMLInputElement || element instanceof HTMLSelectElement ||
    element instanceof HTMLTextAreaElement || element instanceof HTMLButtonElement ? element.labels : null;
  const labelOf = (element: Element): string => {
    const referenced = element.getAttribute("aria-labelledby")?.split(/\s+/)
      .map((id) => document.getElementById(id)?.textContent).join(" ").trim();
    const label = element.getAttribute("aria-label") ?? (referenced || labelsOf(element)?.[0]?.textContent) ??
      (element.matches("input, select, textarea") ? element.getAttribute("placeholder") : element.textContent);
    const tag = element.tagName.toLowerCase();
    return label?.trim() ? `${tag} ${label.trim().replace(/\s+/g, " ")}` : tag;
  };
  const rect = (element: Element): Box => {
    const { x, y, width, height } = element.getBoundingClientRect();
    return { x, y, w: width, h: height };
  };
  const scrollers = [document.documentElement, ...document.body.querySelectorAll("*")];
  const resetHorizontal = (): void => {
    for (const element of scrollers) if (element.scrollLeft !== 0) element.scrollLeft = 0;
  };
  const clipsOf = (element: Element) => {
    const clips = [];
    for (let parent = element.parentElement; parent !== null; parent = parent.parentElement) {
      const { overflowX, overflowY } = getComputedStyle(parent);
      if (overflowX === "visible" && overflowY === "visible") continue;
      const { x, y } = parent.getBoundingClientRect();
      const left = x + parent.clientLeft;
      const top = y + parent.clientTop;
      clips.push({ parent, overflowX, overflowY, left, top, right: left + parent.clientWidth, bottom: top + parent.clientHeight });
    }
    return clips;
  };
  // Keep offscreen siblings comparable, while clipping what could overlap from a different scrollport.
  const bounds = (box: Box, clips: ReturnType<typeof clipsOf>, other?: Element) => {
    let { x: left, y: top } = box;
    let right = left + box.w;
    let bottom = top + box.h;
    for (const clip of clips) {
      if (other !== undefined && clip.parent.contains(other)) continue;
      if (clip.overflowX !== "visible") {
        left = Math.max(left, clip.left);
        right = Math.min(right, clip.right);
      }
      if (clip.overflowY !== "visible") {
        top = Math.max(top, clip.top);
        bottom = Math.min(bottom, clip.bottom);
      }
    }
    return { left, top, right, bottom };
  };
  resetHorizontal();
  const selected = onlyVisible ? elements.filter(visible) : elements;
  const atSameScroll = selected.map((element) => ({ element, box: rect(element), clips: clipsOf(element) }));
  const measured = atSameScroll.map((item) => {
    const { element } = item;
    element.scrollIntoView({ block: "center", inline: "nearest", behavior: "instant" });
    resetHorizontal();
    const box = rect(element);
    const hit = document.elementFromPoint(box.x + box.w / 2, box.y + box.h / 2);
    const isVisible = visible(element) && box.w > 0 && box.h > 0;
    const unoccluded = hit !== null && (element.contains(hit) ||
      [...(labelsOf(element) ?? [])].some((label) => label.contains(hit)));
    const clipped = bounds(box, clipsOf(element));
    const problems: string[] = [];
    if (!isVisible) problems.push("not visible");
    if (box.x < -tolerance || box.y < -tolerance ||
        box.x + box.w > innerWidth + tolerance || box.y + box.h > innerHeight + tolerance) {
      problems.push("outside the window");
    }
    if (clipped.right - clipped.left < box.w - tolerance || clipped.bottom - clipped.top < box.h - tolerance) {
      problems.push("clipped by a scroll container");
    }
    if (!unoccluded) problems.push("covered at its centre");
    const result: MeasuredBox = { label: labelOf(element), ...box, visible: isVisible, unoccluded, problems };
    return { ...item, result };
  });
  for (const [index, first] of measured.entries()) {
    for (const second of measured.slice(index + 1)) {
      if (first.element.contains(second.element) || second.element.contains(first.element)) continue;
      const a = bounds(first.box, first.clips, second.element);
      const b = bounds(second.box, second.clips, first.element);
      const x = Math.min(a.right, b.right) - Math.max(a.left, b.left);
      const y = Math.min(a.bottom, b.bottom) - Math.max(a.top, b.top);
      if (x > tolerance && y > tolerance) {
        first.result.problems.push(`overlaps ${second.result.label} ${JSON.stringify(second.box)} (at the same scroll as ${JSON.stringify(first.box)})`);
      }
    }
  }
  const overflow: string[] = [];
  for (const [element, label] of [
    [document.documentElement, "document"],
    [document.querySelector("main .pages"), "Settings scroll container"],
  ] as const) {
    if (element !== null && element.scrollWidth > element.clientWidth + 1) {
      overflow.push(`${label} has horizontal overflow (scrollWidth=${String(element.scrollWidth)}, clientWidth=${String(element.clientWidth)})`);
    }
  }
  return { boxes: measured.map(({ result }) => result), overflow };
}
