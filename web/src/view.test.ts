import { describe, expect, it } from "vitest";
import { scrollTarget, shouldCollapse, speedOptions, splitLinks, type ScrollInput, type CollapseInput, type TextSegment } from "./view.ts";

const live: CollapseInput = {
  voice: true,
  display: "listening",
  atBottom: true,
  startPinned: false,
  composerFocused: false,
  keyboardOpen: false,
  latestTall: false,
};

describe("shouldCollapse", () => {
  const cases: [string, CollapseInput, boolean][] = [
    ["live voice exchange at the bottom stays big", live, false],
    ["speaking with voice on stays big", { ...live, display: "speaking" }, false],
    ["starting with voice on stays big", { ...live, display: "starting" }, false],
    ["thinking with voice on stays big", { ...live, display: "thinking" }, false],
    ["voice off collapses", { ...live, voice: false }, true],
    ["idle collapses", { ...live, display: "idle" }, true],
    ["connecting collapses", { ...live, display: "connecting" }, true],
    ["reconnecting collapses", { ...live, display: "reconnecting" }, true],
    ["thinking without voice collapses", { ...live, display: "thinking", voice: false }, true],
    ["scrolled up to read collapses", { ...live, atBottom: false }, true],
    ["a pinned message start does not count as scrolling up", { ...live, atBottom: false, startPinned: true }, false],
    ["composer focused collapses", { ...live, composerFocused: true }, true],
    ["keyboard open collapses", { ...live, keyboardOpen: true }, true],
    ["tall latest message collapses", { ...live, latestTall: true }, true],
  ];
  it.each(cases)("%s", (_name, input, expected) => {
    expect(shouldCollapse(input)).toBe(expected);
  });
});

describe("scrollTarget", () => {
  const following: ScrollInput = { nearBottom: true, latestTall: false, pinned: false, latestHandled: false, latestPinned: false };
  const cases: [string, ScrollInput, string][] = [
    ["follows the bottom while the latest message is short", following, "bottom"],
    ["keeps the start of a latest message that has just grown taller than the viewport", { ...following, latestTall: true }, "start"],
    ["keeps the start in view while the pinned message keeps growing", { ...following, nearBottom: false, pinned: true, latestTall: true, latestHandled: true, latestPinned: true }, "start"],
    ["follows the bottom once the user scrolled down to a message already pinned", { ...following, latestTall: true, latestHandled: true }, "bottom"],
    ["follows the bottom into the next turn after a pinned reply", { ...following, nearBottom: false, pinned: true }, "bottom"],
    ["pins the start of a tall next turn after a pinned reply", { ...following, nearBottom: false, pinned: true, latestTall: true }, "start"],
    ["leaves a user who scrolled up where they are", { ...following, nearBottom: false, latestTall: true, latestHandled: true }, "stay"],
    ["leaves a user who scrolled up from a short message", { ...following, nearBottom: false }, "stay"],
  ];
  it.each(cases)("%s", (_name, input, expected) => {
    expect(scrollTarget(input)).toBe(expected);
  });
});

describe("speedOptions", () => {
  it("lists every step in order with its label", () => {
    expect(speedOptions(1).map((o) => o.label)).toEqual([
      "0.75×",
      "0.9×",
      "1×",
      "1.1×",
      "1.25×",
      "1.5×",
      "1.75×",
      "2×",
    ]);
    expect(speedOptions(1).map((o) => o.rate)).toEqual([0.75, 0.9, 1, 1.1, 1.25, 1.5, 1.75, 2]);
  });

  it.each([0.75, 1, 1.25, 2] as const)("marks only %s as selected", (rate) => {
    const selected = speedOptions(rate).filter((o) => o.selected);
    expect(selected).toEqual([{ rate, label: `${String(rate)}×`, selected: true }]);
  });

});

describe("splitLinks", () => {
  const cases: [string, string, TextSegment[]][] = [
    ["plain text", "hello world", [{ kind: "text", text: "hello world" }]],
    ["empty", "", []],
    [
      "url in the middle",
      "see https://example.com/recipes now",
      [
        { kind: "text", text: "see " },
        { kind: "link", text: "https://example.com/recipes", href: "https://example.com/recipes" },
        { kind: "text", text: " now" },
      ],
    ],
    [
      "trailing full stop stays text",
      "open https://example.com/a.",
      [
        { kind: "text", text: "open " },
        { kind: "link", text: "https://example.com/a", href: "https://example.com/a" },
        { kind: "text", text: "." },
      ],
    ],
    [
      "url in brackets",
      "(https://example.com/a)",
      [
        { kind: "text", text: "(" },
        { kind: "link", text: "https://example.com/a", href: "https://example.com/a" },
        { kind: "text", text: ")" },
      ],
    ],
    [
      "www url gets an https href",
      "go to www.example.com/x",
      [
        { kind: "text", text: "go to " },
        { kind: "link", text: "www.example.com/x", href: "https://www.example.com/x" },
      ],
    ],
    [
      "two urls",
      "https://a.example and https://b.example/y",
      [
        { kind: "link", text: "https://a.example", href: "https://a.example" },
        { kind: "text", text: " and " },
        { kind: "link", text: "https://b.example/y", href: "https://b.example/y" },
      ],
    ],
    [
      "line breaks are kept in text",
      "one\n\ntwo https://example.com",
      [
        { kind: "text", text: "one\n\ntwo " },
        { kind: "link", text: "https://example.com", href: "https://example.com" },
      ],
    ],
    ["long word without a url is untouched", "a".repeat(80), [{ kind: "text", text: "a".repeat(80) }]],
    ["bare domain is not a link", "visit example.com today", [{ kind: "text", text: "visit example.com today" }]],
  ];
  it.each(cases)("%s", (_name, input, expected) => {
    expect(splitLinks(input)).toEqual(expected);
  });
});
