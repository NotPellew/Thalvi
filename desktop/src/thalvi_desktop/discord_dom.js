// Owns the bounded Discord composer observation and its one-shot input gate.
// It never reads history, tokens or anything outside the editor's form region.
(() => {
  "use strict";

  const EDITOR = 'div[role="textbox"][contenteditable="true"][data-slate-editor="true"][data-slate-node="value"]';
  const ORIGIN = "https://discord.com";
  const GATE_KEY = "__thalvi_discord_gate__";
  const GATE_DEFAULT_KEY = "__thalvi_discord_gate_default__";
  const GATE_LIFETIME = 10000;
  const KNOWN_CONTROLS = new Set([
    "More message options",
    "Send a gift",
    "Open GIF picker",
    "Open sticker picker",
    "Add Emoji",
    "Apps",
  ]);
  const OPTIONAL_CONTROLS = new Set(["Send Message", "Send a message"]);
  const ALLOWED_ROLES = new Set(["textbox", "button", "img"]);
  const ALLOWED_TAGS = new Set([
    "FORM",
    "DIV",
    "SPAN",
    "SVG",
    "PATH",
    "G",
    "USE",
    "RECT",
    "CIRCLE",
    "ELLIPSE",
    "LINE",
    "POLYLINE",
    "POLYGON",
    "DEFS",
    "CLIPPATH",
    "FILTER",
    "FECOMPONENTTRANSFER",
    "FEFUNCA",
    "MASK",
    "TITLE",
    "INPUT",
    "BUTTON",
    "BR",
  ]);
  const RETRYABLE = new Set(["editor-missing", "editor-hidden", "route-mismatch"]);

  const normalize = (value) => (value || "").replace(/\s+/g, " ").trim();

  const fail = (reason, detail) => ({
    ok: false,
    reason,
    detail: detail || null,
    retryable: RETRYABLE.has(reason),
    draft: "",
    digest: "",
    focused: false,
    route: "",
  });

  const visible = (element) => {
    const rect = element.getBoundingClientRect();
    return rect.width > 0 && rect.height > 0;
  };

  const isControl = (element) => element.getAttribute("role") === "button";

  const controlLabel = (element) => normalize(element.getAttribute("aria-label"));

  const isKnownControl = (element) =>
    isControl(element) &&
    (KNOWN_CONTROLS.has(controlLabel(element)) || OPTIONAL_CONTROLS.has(controlLabel(element)));

  const findEditor = () => document.querySelector(EDITOR);

  const draftOf = (editor) => {
    const copy = editor.cloneNode(true);
    for (const placeholder of copy.querySelectorAll("[data-slate-placeholder]")) {
      placeholder.remove();
    }
    if (copy.querySelector("img, video, audio, canvas, iframe, object, embed")) {
      return null;
    }
    for (const sentinel of copy.querySelectorAll("[data-slate-zero-width]")) {
      if (sentinel.textContent !== "\ufeff") continue;
      const kind = sentinel.getAttribute("data-slate-zero-width");
      if (kind === "z" && sentinel.childElementCount === 0) {
        sentinel.remove();
        continue;
      }
      const block = sentinel.closest('[data-slate-node="element"]');
      const emptyBreak = block && block.textContent === "\ufeff" &&
        sentinel.childElementCount === 1 && sentinel.firstElementChild.tagName === "BR";
      if (emptyBreak && (
        kind === "z" || (kind === "n" && sentinel.getAttribute("data-slate-length") === "0")
      )) sentinel.remove();
    }
    if (copy.querySelector("br")) return null;
    const blocks = [...copy.children];
    if (blocks.length && blocks.every((child) => child.getAttribute("data-slate-node") === "element")) {
      return blocks.map((block) => block.textContent).join("\n");
    }
    return copy.textContent;
  };

  const knownHintTargets = (form, editor) => {
    const targets = new Set();
    for (const element of form.querySelectorAll("[aria-describedby]")) {
      if (element === editor || element.contains(editor)) continue;
      const controls = isControl(element)
        ? [element]
        : [...element.querySelectorAll('[role="button"]')];
      if (controls.length !== 1 || controlLabel(controls[0]) !== "Open GIF picker") continue;
      for (const id of (element.getAttribute("aria-describedby") || "").split(/\s+/)) {
        if (!id) continue;
        const target = document.getElementById(id);
        if (
          target && form.contains(target) && target.tagName === "SPAN" &&
          target.childElementCount === 0 && target.getAttribute("role") === null &&
          target.textContent === "Send GIF"
        ) targets.add(target);
      }
    }
    return targets;
  };

  const digestOf = (form, editor) => {
    const parts = [];
    const walk = (element) => {
      if (element === editor) {
        parts.push("E");
        return;
      }
      if (element.closest("[data-slate-placeholder]")) return;

      const attributes = [];
      for (const name of [
        "role",
        "type",
        "multiple",
        "aria-expanded",
        "aria-pressed",
        "aria-checked",
        "aria-hidden",
        "aria-live",
        "aria-describedby",
        "id",
      ]) {
        if (element.hasAttribute(name)) attributes.push(`${name}=${element.getAttribute(name)}`);
      }
      const label = normalize(element.getAttribute("aria-label"));
      if (label) attributes.push(`label=${label}`);
      parts.push(`${element.tagName.toUpperCase()}[${attributes.join(",")}]`);
      for (const node of element.childNodes) {
        if (node.nodeType === Node.TEXT_NODE && normalize(node.nodeValue)) {
          parts.push(`text=${node.nodeValue}`);
        }
      }
      for (const child of element.children) walk(child);
    };
    walk(form);
    return parts.join(">");
  };

  const observe = (editor, route) => {
    if (!editor || !editor.isConnected) return fail("editor-missing");
    if (document.querySelectorAll(EDITOR).length !== 1) return fail("editor-ambiguous");
    if (!visible(editor)) return fail("editor-hidden");
    if (location.origin !== ORIGIN) return fail("route-mismatch");
    if (location.origin + location.pathname !== route) return fail("route-mismatch");
    if (location.search || location.hash) return fail("route-mismatch");
    const label = normalize(editor.getAttribute("aria-label"));
    if (!/^Message(?:\s|$)/.test(label)) return fail("editor-label");

    const form = editor.closest("form");
    if (!form || !form.contains(editor)) return fail("structure", "no-form");
    if (form.querySelectorAll(EDITOR).length !== 1) return fail("editor-ambiguous");

    for (const element of form.querySelectorAll("*")) {
      const tag = element.tagName.toUpperCase();
      if (!ALLOWED_TAGS.has(tag)) {
        return fail("structure", `tag=${tag}`);
      }
      const editable = element.getAttribute("contenteditable");
      if (editable !== null && element !== editor && editable !== "false") {
        return fail("structure", "editable");
      }
      if (element.getAttribute("aria-expanded") === "true") return fail("menu-open");
      if (element.getAttribute("aria-pressed") === "true") return fail("menu-open");
      if (element.getAttribute("aria-checked") === "true") return fail("menu-open");
      const role = element.getAttribute("role");
      if (role === null) continue;
      if (!ALLOWED_ROLES.has(role)) return fail("structure", `role=${role}`);
      if (role === "textbox" && element !== editor) return fail("structure", "extra-textbox");
      if (role === "img" && tag !== "SVG") {
        return fail("structure", "role-img");
      }
      if (role === "button" && !isKnownControl(element)) return fail("control", "unlisted");
    }

    for (const element of document.querySelectorAll(
      '[role="dialog"], [role="menu"], [role="listbox"], [aria-modal="true"]',
    )) {
      if (visible(element)) return fail("dialog");
    }

    const inputs = [...form.querySelectorAll("input")];
    if (inputs.length !== 1) return fail("structure", "inputs");
    const upload = inputs[0];
    if ((upload.getAttribute("type") || "").toLowerCase() !== "file") {
      return fail("structure", "input-type");
    }
    if (upload.files && upload.files.length > 0) return fail("files");

    const hints = knownHintTargets(form, editor);
    for (const element of form.querySelectorAll("*")) {
      if (element === editor || editor.contains(element)) continue;
      if (element.closest("[data-slate-placeholder]")) continue;
      if (isControl(element) || element.tagName === "BUTTON") continue;
      if (hints.has(element)) continue;
      for (const node of element.childNodes) {
        if (node.nodeType === Node.TEXT_NODE && normalize(node.nodeValue)) {
          return fail("text", "unexpected");
        }
      }
    }

    const draft = draftOf(editor);
    if (draft === null) return fail("structure", "rich-content");
    return {
      ok: true,
      reason: null,
      detail: null,
      retryable: false,
      draft,
      digest: digestOf(form, editor),
      focused: document.activeElement === editor,
      route: location.origin + location.pathname,
    };
  };

  const verify = (editor, route, draft, digest) => {
    const observation = observe(editor, route);
    if (!observation.ok) return observation;
    if (!observation.focused) return { ...observation, ok: false, reason: "editor-unfocused" };
    if (observation.draft !== draft) {
      return { ...observation, ok: false, reason: "draft-mismatch", retryable: true };
    }
    if (digest && observation.digest !== digest) {
      return { ...observation, ok: false, reason: "structure-changed" };
    }
    return observation;
  };

  const selectionInside = (editor) => {
    const selection = document.getSelection();
    if (!selection || selection.rangeCount !== 1 || !selection.isCollapsed) return false;
    const node = selection.anchorNode;
    return node !== null && (node === editor || editor.contains(node));
  };

  const clearGate = () => {
    const fallback = window[GATE_DEFAULT_KEY];
    if (fallback && fallback.listener) {
      document.removeEventListener("beforeinput", fallback.listener, true);
      delete window[GATE_DEFAULT_KEY];
    }
    const gate = window[GATE_KEY];
    if (!gate) return;
    if (gate.listener) document.removeEventListener("beforeinput", gate.listener, true);
    delete window[GATE_KEY];
  };

  const arm = (editor, route, draft, digest, text, nonce) => {
    const current = verify(editor, route, draft, digest);
    if (!current.ok) return current;
    if (!selectionInside(editor)) {
      return { ...current, ok: false, reason: "editor-unfocused", focused: false };
    }
    const prior = window[GATE_KEY];
    if (prior && (prior.state === "armed" || prior.unresolved)) {
      return { ...current, ok: false, reason: "gate-pending" };
    }
    clearGate();
    const gate = {
      nonce,
      text,
      state: "armed",
      reason: null,
      events: 0,
      expires: performance.now() + GATE_LIFETIME,
      listener: null,
    };
    gate.listener = (event) => {
      if (!event.isTrusted) return;
      if (event.inputType !== "insertText" || event.data !== text) return;
      gate.events += 1;
      if (gate.state !== "armed") {
        if (gate.state === "allowed") {
          gate.state = "denied";
          gate.reason = "gate-consumed";
        }
        event.preventDefault();
        event.stopImmediatePropagation();
        return;
      }
      let reason = null;
      if (performance.now() > gate.expires) {
        reason = "gate-expired";
      } else if (event.target !== editor) {
        reason = "target-mismatch";
      } else {
        let observed = null;
        try {
          observed = verify(editor, route, draft, digest);
        } catch {
          reason = "structure";
        }
        if (reason === null && (!observed || !observed.ok)) {
          reason = observed ? observed.reason || "structure" : "structure";
        }
        if (reason === null && !selectionInside(editor)) reason = "editor-unfocused";
      }
      if (reason !== null) {
        gate.state = "denied";
        gate.reason = reason;
        event.preventDefault();
        event.stopImmediatePropagation();
        return;
      }
      gate.state = "allowed";
    };
    document.addEventListener("beforeinput", gate.listener, true);
    window[GATE_KEY] = gate;
    return current;
  };

  const gateOutcome = (nonce) => {
    const gate = window[GATE_KEY];
    if (!gate || gate.nonce !== nonce) return { state: "missing", reason: null, events: 0 };
    return { state: gate.state, reason: gate.reason, events: gate.events };
  };

  const denyPending = (nonce) => {
    const gate = window[GATE_KEY];
    if (!gate || gate.nonce !== nonce || gate.state !== "armed") return false;
    gate.state = "denied";
    gate.reason = "cancelled";
    gate.unresolved = true;
    return true;
  };

  const disarm = (nonce) => {
    const gate = window[GATE_KEY];
    const fallback = window[GATE_DEFAULT_KEY];
    if (gate && gate.unresolved) return false;
    if (gate && gate.nonce !== nonce) return false;
    if (!gate && !fallback) return false;
    if (!gate && fallback.nonce !== nonce) return false;
    clearGate();
    return true;
  };

  const enter = (editor, route, draft, digest) => {
    const current = verify(editor, route, draft, digest);
    if (!current.ok) return current;
    editor.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Enter", code: "Enter", keyCode: 13, which: 13, bubbles: true, cancelable: true,
    }));
    return current;
  };

  return { findEditor, observe, verify, arm, gateOutcome, denyPending, disarm, enter };
})();
