import React, { useEffect, useMemo, useState } from "react";
import { createRoot } from "react-dom/client";
import { flushSync } from "react-dom";
import { createEditor, Node } from "slate";
import { Slate, Editable, withReact } from "slate-react";

const EDITOR =
  'div[role="textbox"][contenteditable="true"][data-slate-editor="true"][data-slate-node="value"]';
const INITIAL = [{ type: "paragraph", children: [{ text: "" }] }];
const fixture = (window.__fixture = {
  changes: [],
  events: [],
  submits: [],
  ready: false,
});

document.addEventListener(
  "beforeinput",
  (event) => {
    fixture.events.push({
      trusted: event.isTrusted,
      inputType: event.inputType,
      data: event.data,
      cancelable: event.cancelable,
      target: event.target?.getAttribute?.("data-slate-editor") === "true",
    });
  },
  true,
);

const rendered = () => {
  const editor = document.querySelector(EDITOR);
  if (!editor) return "";
  const copy = editor.cloneNode(true);
  copy.querySelectorAll("[data-slate-placeholder]").forEach((node) => node.remove());
  copy.querySelectorAll("[data-slate-zero-width]").forEach((node) => {
    if (node.textContent === "\ufeff") node.remove();
  });
  return copy.textContent;
};

function App() {
  const editor = useMemo(() => withReact(createEditor()), []);
  const [value, setValue] = useState(INITIAL);
  const [draft, setDraft] = useState("");
  const [revision, setRevision] = useState(0);
  const [editorKey, setEditorKey] = useState(0);

  useEffect(() => {
    document.querySelector(EDITOR)?.focus();
    fixture.ready = true;
  }, [editorKey]);

  Object.assign(fixture, {
    editor,
    model: () => Node.string(editor),
    rendered,
    focus: () => document.querySelector(EDITOR)?.focus(),
    rerender: () => {
      flushSync(() => setRevision((current) => current + 1));
      return true;
    },
    replaceEditor: () => {
      flushSync(() => setEditorKey((current) => current + 1));
      document.querySelector(EDITOR)?.focus();
    },
    setDraft: (text) => {
      editor.insertText(text);
    },
  });

  return (
    <Slate
      editor={editor}
      initialValue={INITIAL}
      onChange={(next) => {
        fixture.changes.push(Node.string(editor));
        setValue(next);
        setDraft(Node.string(editor));
      }}
    >
      <form
        data-revision={revision}
        onSubmit={(event) => {
          event.preventDefault();
          fixture.submits.push(Node.string(editor));
        }}
      >
        <div>
          <Editable
            key={editorKey}
            aria-label="Message #demo"
            placeholder="Message #demo"
            style={{ minHeight: "48px", minWidth: "320px" }}
            onKeyDown={(event) => {
              if (event.key === "Enter" && !event.shiftKey) {
                event.preventDefault();
                fixture.submits.push(Node.string(editor));
              }
            }}
          />
        </div>
        <div>
          <button
            type="button"
            role="button"
            aria-label="More message options"
            aria-expanded="false"
          />
          <div aria-describedby="gif-hint">
            <button
              type="button"
              role="button"
              aria-label="Open GIF picker"
              aria-expanded="false"
              aria-haspopup="dialog"
            />
          </div>
          <span id="gif-hint">Send GIF</span>
          <button
            type="button"
            role="button"
            aria-label="Open sticker picker"
            aria-expanded="false"
            aria-haspopup="dialog"
          />
          <button type="button" role="button" aria-label="Add Emoji" aria-expanded="false" />
          <button
            type="button"
            role="button"
            aria-label="Apps"
            aria-expanded="false"
            aria-haspopup="dialog"
          />
        </div>
        <input
          type="file"
          multiple
          aria-hidden="true"
          tabIndex={-1}
          style={{ display: "none" }}
        />
        <div style={{ display: draft ? "block" : "none" }}>
          <button type="button" role="button" aria-label="Send Message" />
        </div>
        <span aria-live="polite" aria-atomic="true" />
      </form>
    </Slate>
  );
}

createRoot(document.getElementById("root")).render(<App />);
