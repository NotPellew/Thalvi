// Default-denies the exact agent input in documents created after arming.
// A current-document gate, when present, makes the decision instead.
(() => {
  "use strict";
  const body = globalThis.__thalvi_gate_body__;
  const nonce = globalThis.__thalvi_gate_nonce__;
  delete globalThis.__thalvi_gate_body__;
  delete globalThis.__thalvi_gate_nonce__;
  if (typeof body !== "string" || typeof nonce !== "string") return;
  const KEY = "__thalvi_discord_gate__";
  const deny = (event) => {
    if (!event.isTrusted) return;
    if (event.inputType !== "insertText" || event.data !== body) return;
    const gate = globalThis[KEY];
    if (gate && gate.state === "armed" && gate.listener) return;
    event.preventDefault();
    event.stopImmediatePropagation();
    globalThis[KEY] = {
      nonce,
      text: body,
      state: "denied",
      reason: "document-changed",
      events: 1,
      expires: 0,
      listener: deny,
    };
  };
  document.addEventListener("beforeinput", deny, true);
  globalThis.__thalvi_discord_gate_default__ = { nonce, listener: deny };
})();
