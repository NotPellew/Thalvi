// @vitest-environment jsdom
// @vitest-environment-options {"url": "https://discord.com/channels/111/222"}
import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

const source = readFileSync(
  resolve(globalThis.process.cwd(), "../desktop/src/thalvi_desktop/discord_dom.js"),
  "utf8",
);

const ROUTE = "https://discord.com/channels/111/222";
const guard = (0, eval)(source);

const BASELINE = `
<form>
  <div>
    <div>
      <div>
        <div style="display: none"></div>
        <div>
          <div>
            <div style="display: none">
              <input type="file" multiple aria-hidden="true" tabindex="-1">
            </div>
            <div>
              <div role="button" aria-label="More message options" aria-expanded="false"
                   aria-haspopup="menu" tabindex="0">
                <div><svg role="img" width="20" height="20"><path d="M0 0"></path></svg></div>
              </div>
            </div>
            <div>
              <div>
                <div data-slate-placeholder="true" aria-hidden="true">Message #demo</div>
                <div role="textbox" contenteditable="true" data-slate-editor="true"
                     data-slate-node="value" aria-label="Message #demo" aria-multiline="true"
                     spellcheck="true" aria-haspopup="listbox" aria-invalid="false"
                     aria-autocomplete="list" autocomplete="off" autocorrect="off"
                     data-can-focus="true">
                  <div data-slate-node="element"><span data-slate-node="text"><span data-slate-zero-width="n" data-slate-length="0">\ufeff<br></span></span></div>
                </div>
              </div>
            </div>
            <div>
              <div><div role="button" aria-label="Send a gift" tabindex="0">
                <div></div>
              </div></div>
              <div aria-describedby="hint-1">
                <div role="button" aria-label="Open GIF picker" aria-expanded="false"
                     aria-haspopup="dialog" aria-disabled="false" tabindex="0"></div>
              </div>
              <span id="hint-1">Send GIF</span>
              <div><div role="button" aria-label="Open sticker picker" aria-expanded="false"
                        aria-haspopup="dialog" tabindex="0"></div></div>
              <div><div role="button" aria-label="Add Emoji" aria-expanded="false"
                        tabindex="0"></div></div>
              <div><div role="button" aria-label="Apps" aria-expanded="false"
                        aria-haspopup="dialog" tabindex="0"></div></div>
            </div>
            <div class="send-slot" style="display: none">
              <div role="button" aria-label="Send Message" tabindex="0"></div>
            </div>
          </div>
        </div>
      </div>
    </div>
    <span aria-live="polite" aria-atomic="true"></span>
  </div>
</form>
`;

function editor(container = document) {
  return container.querySelector(
    'div[role="textbox"][contenteditable="true"][data-slate-editor="true"][data-slate-node="value"]',
  );
}

function focus(element) {
  Object.defineProperty(document, "activeElement", { configurable: true, value: element });
}

function select(element) {
  const selection = document.getSelection();
  selection.removeAllRanges();
  const range = document.createRange();
  range.selectNodeContents(element);
  range.collapse(true);
  selection.addRange(range);
}

function mount(mutate) {
  document.body.innerHTML = BASELINE;
  if (mutate) mutate(document.body);
  const element = editor();
  focus(element);
  return element;
}

beforeEach(() => {
  vi.spyOn(Element.prototype, "getBoundingClientRect").mockImplementation(function rect() {
    const style = window.getComputedStyle(this);
    const hidden = style.display === "none" || style.visibility === "hidden";
    const size = hidden ? 0 : 10;
    const value = { x: 0, y: 0, width: size, height: size, top: 0, right: size, bottom: size, left: 0 };
    return { ...value, toJSON: () => value };
  });
});

afterEach(() => {
  if (window.__thalvi_discord_gate__) {
    guard.disarm(window.__thalvi_discord_gate__.nonce);
  }
  vi.restoreAllMocks();
  delete document.activeElement;
});

describe("Discord composer guard", () => {
  it("recognizes the observed empty composer and its placeholder as a baseline", () => {
    const element = mount();
    const result = guard.observe(element, ROUTE);
    expect(result.ok).toBe(true);
    expect(result.draft).toBe("");
    expect(result.focused).toBe(true);
    expect(result.digest).toContain("INPUT[type=file,multiple=,aria-hidden=true]");
    expect(guard.verify(element, ROUTE, "", result.digest).ok).toBe(true);
  });

  it("reports a populated draft instead of treating it as empty", () => {
    const element = mount();
    element.textContent = "Hello there";
    const result = guard.observe(element, ROUTE);
    expect(result.ok).toBe(true);
    expect(result.draft).toBe("Hello there");
    const verified = guard.verify(element, ROUTE, "", result.digest);
    expect(verified.ok).toBe(false);
    expect(verified.reason).toBe("draft-mismatch");
  });

  it("accepts the exact inserted draft", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    element.textContent = "Hello there";
    expect(guard.verify(element, ROUTE, "Hello there", baseline.digest).ok).toBe(true);
  });

  it.each([
    ["First<br>Second", "FirstSecond"],
    ["Hello<br>", "Hello"],
  ])("refuses a non-sentinel inline break in %s", (markup, approved) => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    element.innerHTML = `<div data-slate-node="element">${markup}</div>`;
    expect(guard.verify(element, ROUTE, approved, baseline.digest).ok).toBe(false);
  });

  it.each(["z", "n"])("refuses a %s break marker after authored text", (kind) => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    element.innerHTML = `<div data-slate-node="element">Hello<span data-slate-zero-width="${kind}" data-slate-length="0">\ufeff<br></span></div>`;
    expect(guard.verify(element, ROUTE, "Hello", baseline.digest).ok).toBe(false);
  });

  it("allows the known send control to appear with a draft", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    document.querySelector(".send-slot").style.display = "block";
    element.textContent = "Hello";
    const result = guard.verify(element, ROUTE, "Hello", baseline.digest);
    expect(result.ok).toBe(true);
  });

  it("recognizes Send GIF only as the GIF control's static hint", () => {
    const element = mount();
    const hint = document.querySelector("#hint-1");
    hint.textContent = "Send GIF";
    expect(guard.observe(element, ROUTE).ok).toBe(true);
    hint.textContent = "Replying to Alice";
    expect(guard.observe(element, ROUTE).reason).toBe("text");
    hint.textContent = "Send GIF";
    document.querySelector('[aria-describedby="hint-1"]').removeAttribute("aria-describedby");
    element.setAttribute("aria-describedby", "hint-1");
    expect(guard.observe(element, ROUTE).reason).toBe("text");
  });

  it("refuses an unlisted control without echoing its label", () => {
    const element = mount((body) => {
      body.querySelector('div[aria-label="Send a gift"]').parentElement.innerHTML =
        '<div role="button" aria-label="Secret control"></div>';
    });
    const result = guard.observe(element, ROUTE);
    expect(result.ok).toBe(false);
    expect(result.reason).toBe("control");
    expect(JSON.stringify(result)).not.toContain("Secret");
  });

  it("refuses an open menu", () => {
    const element = mount((body) => {
      body.querySelector('div[aria-label="Add Emoji"]').setAttribute("aria-expanded", "true");
    });
    expect(guard.observe(element, ROUTE).reason).toBe("menu-open");
  });

  it("refuses a visible dialog outside the composer", () => {
    const element = mount((body) => {
      body.insertAdjacentHTML("beforeend", '<div role="dialog">Confirm</div>');
    });
    expect(guard.observe(element, ROUTE).reason).toBe("dialog");
  });

  it("refuses an attachment input and a rendered attachment", () => {
    const element = mount((body) => {
      const input = body.querySelector('input[type="file"]');
      Object.defineProperty(input, "files", { configurable: true, value: [new File(["x"], "x")] });
    });
    expect(guard.observe(element, ROUTE).reason).toBe("files");

    const second = mount((body) => {
      body.querySelector("form").insertAdjacentHTML("beforeend", '<img src="attachment">');
    });
    expect(guard.observe(second, ROUTE).reason).toBe("structure");
  });

  it("refuses reply, edit and other unexpected text banners", () => {
    const element = mount((body) => {
      body.querySelector("form").insertAdjacentHTML(
        "afterbegin",
        "<div>Replying to someone</div>",
      );
    });
    expect(guard.observe(element, ROUTE).reason).toBe("text");
  });

  it("refuses rich content inside the editor", () => {
    const element = mount((body) => {
      editor(body).innerHTML = '<div data-slate-node="element"><img src="embed"></div>';
    });
    expect(guard.observe(element, ROUTE).reason).toBe("structure");
  });

  it("refuses an extra editable region", () => {
    const element = mount((body) => {
      body.querySelector("form").insertAdjacentHTML(
        "beforeend",
        '<div contenteditable="true">Another editor</div>',
      );
    });
    expect(guard.observe(element, ROUTE).reason).toBe("structure");
  });

  it("refuses another message box", () => {
    const element = mount((body) => {
      const clone = editor(body).cloneNode(true);
      body.querySelector("form").append(clone);
    });
    expect(guard.observe(element, ROUTE).reason).toBe("editor-ambiguous");
  });

  it("refuses a hidden editor", () => {
    const element = mount((body) => {
      editor(body).style.display = "none";
    });
    expect(guard.observe(element, ROUTE).reason).toBe("editor-hidden");
  });

  it("refuses a route change", () => {
    const element = mount();
    expect(guard.observe(element, "https://discord.com/channels/1/2").reason).toBe(
      "route-mismatch",
    );
  });

  it("refuses lost focus at the confirmation barrier", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    focus(document.body);
    expect(guard.verify(element, ROUTE, "", baseline.digest).reason).toBe("editor-unfocused");
  });

  it("refuses structure added after the baseline", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    document.querySelector("form").insertAdjacentHTML("beforeend", "<div></div>");
    expect(guard.verify(element, ROUTE, "", baseline.digest).reason).toBe("structure-changed");
  });

  it("refuses a replaced editor handle", () => {
    const element = mount();
    const replacement = element.cloneNode(true);
    element.replaceWith(replacement);
    focus(replacement);
    expect(guard.observe(element, ROUTE).reason).toBe("editor-missing");
  });
});

describe("Discord composer guarded actions", () => {
  it("refuses new described or live reply context", () => {
    for (const kind of ["described", "live"]) {
      const element = mount();
      const baseline = guard.observe(element, ROUTE);
      if (kind === "described") {
        const context = document.createElement("div");
        context.id = "reply-context";
        context.textContent = "Replying to Alice";
        document.querySelector("form").append(context);
        element.setAttribute("aria-describedby", "reply-context");
      } else {
        document.querySelector('[aria-live]').textContent = "Replying to Alice";
      }
      expect(guard.verify(element, ROUTE, "", baseline.digest).ok).toBe(false);
    }
  });

  it("compares exact text including whitespace, zero width characters and Slate block boundaries", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    for (const value of [" Hello ", "\u200bHello", "\nHello\n"]) {
      element.textContent = value;
      expect(guard.verify(element, ROUTE, "Hello", baseline.digest).ok).toBe(false);
    }
    element.innerHTML = '<div data-slate-node="element">First</div><div data-slate-node="element">Second</div>';
    expect(guard.observe(element, ROUTE).draft).toBe("First\nSecond");
    element.innerHTML = '<div data-slate-node="element"><span data-slate-zero-width="z">\ufeff<br></span></div>';
    expect(guard.observe(element, ROUTE).draft).toBe("");
  });

  it("checks state inside enter before dispatching", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    focus(document.body);
    const listener = vi.fn();
    element.addEventListener("keydown", listener);
    expect(guard.enter(element, ROUTE, "", baseline.digest).ok).toBe(false);
    expect(listener).not.toHaveBeenCalled();
  });

  it("arms only with focus and a collapsed selection inside the editor", () => {
    const element = mount();
    const baseline = guard.observe(element, ROUTE);
    focus(document.body);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "Hello", "nonce-1").ok).toBe(false);
    expect(window.__thalvi_discord_gate__).toBeUndefined();
    focus(element);
    select(document.body);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "Hello", "nonce-1").reason).toBe(
      "editor-unfocused",
    );
    select(element);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "Hello", "nonce-1").ok).toBe(true);
  });

  it("retains an unresolved authorization until the document is replaced", () => {
    const element = mount();
    select(element);
    const baseline = guard.observe(element, ROUTE);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "Hello", "nonce-1").ok).toBe(true);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "Hello", "nonce-2").reason).toBe(
      "gate-pending",
    );
    expect(guard.gateOutcome("nonce-2").state).toBe("missing");
    expect(guard.gateOutcome("nonce-1").state).toBe("armed");
    expect(guard.denyPending("nonce-1")).toBe(true);
    expect(guard.gateOutcome("nonce-1")).toEqual({
      state: "denied",
      reason: "cancelled",
      events: 0,
    });
    expect(guard.disarm("nonce-1")).toBe(false);
    expect(guard.arm(element, ROUTE, "", baseline.digest, "new", "nonce-2").reason).toBe(
      "gate-pending",
    );
  });
});
