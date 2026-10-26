import { describe, expect, it } from "vitest";
import { shouldCollapse, splitLinks, type CollapseInput, type TextSegment } from "./view.ts";

const live: CollapseInput = {
  voice: true,
  display: "listening",
  atBottom: true,
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
    ["composer focused collapses", { ...live, composerFocused: true }, true],
    ["keyboard open collapses", { ...live, keyboardOpen: true }, true],
    ["tall latest message collapses", { ...live, latestTall: true }, true],
  ];
  it.each(cases)("%s", (_name, input, expected) => {
    expect(shouldCollapse(input)).toBe(expected);
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
