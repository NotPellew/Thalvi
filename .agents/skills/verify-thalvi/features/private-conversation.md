# Private conversation

The user starts a conversation that leaves nothing behind: it is never saved,
listed or searchable from other conversations, a Private badge stays visible
while it is open, and it ends for good when the connection drops instead of
reconnecting into a saved conversation.

## Sub-features

- `private-start` opens a private conversation from `New private
  conversation`, shows the `Private` badge and its note, and keeps its id out
  of browser storage.
- `private-unsaved` stores no turns: no session file, no `sessions` entry, no
  row in the Conversations sheet, no transcript text anywhere under `R/data/`.
  Only a metrics recording is written.
- `private-search` lets `search_history` inside the conversation find its own
  turns, titled `Current conversation`.
- `private-reload` starts an ordinary empty conversation after a reload.
- `private-drop` ends the conversation on a lost connection with a plain
  notice and opens no new socket.

## How to get to it (user POV)

- Choose `Conversations`, then `New private conversation`. The line under the
  two buttons says what private does and does not cover.

## Driving it with thalvi-verify

Preconditions:

- Baseline from `features/README.md`, page at `idle`.
- At least one ordinary saved conversation, so the sheet has rows to compare
  against. Send `A saved turn first` and run
  `$V reply $R --after "A saved turn first"`.

Steps:

- **Start.** Choose `Conversations`, then `New private conversation`.
  - First wrap the socket so **Drop** can close it later:
    `$V ab $R eval 'const W = window.WebSocket; window.__ws = []; window.WebSocket = class extends W { constructor(...a) { super(...a); window.__ws.push(this); } }; 1'`.
  - Run `$V ab $R click 'button[aria-label="Conversations"]'`,
    `$V ab $R click '.o-lib-new-private'` and `$V state $R idle`.
  - `.o-private` is not hidden and its `textContent` is `Private` (CSS
    uppercases what `innerText` returns); `.o-private-note` reads
    `Not saved, and gone when you leave or reload. Providers still process
    your requests.`
  - `window.__ws.at(-1).url` ends in `/session?private=1`, and
    `JSON.stringify(Object.entries(sessionStorage))` is `[]`.
- **Talk.** Send `Keep this secret about the garden` and run
  `$V reply $R --after "Keep this secret about the garden"`.
- **Search inside.** Send `search for garden` and run
  `$V reply $R --after "search for garden"`.
  - The last `llm_request` in `R/fakes.jsonl` has `tool_results` whose matches
    carry a `private-…` `session_id` and the title `Current conversation`.
- **Nothing saved.** Check the server side.
  - `ls R/data/sessions/` and the ids and titles from `curl <URL>sessions`
    are unchanged from before **Start**. Only `age_s` moves.
  - `grep -rl garden R/data` prints nothing.
  - `R/data/recordings/<date>/private-*.jsonl` exists and holds metrics only.
  - Open `Conversations`, wait for a `Delete …` button, and confirm
    `.o-library` has no `garden` text. Close the sheet.
- **Drop.** Lose the connection.
  - Run `$V ab $R eval 'window.__ws.at(-1).close(); 1'` and
    `$V state $R ended`.
  - `.o-notice-text` reads `This private conversation ended when the
    connection dropped, and can't be resumed. Start a new conversation.`, and
    `window.__ws.length` is still the count from before the drop.
- **Reload.** Run `$V ab $R reload` and `$V state $R idle`.
  - `.o-private` is hidden and `.o-turns` is empty: an ordinary new
    conversation, not the private one.
- **Proof.** Screenshot and ARIA-snapshot after **Search inside** and after
  **Drop**. Keep the `sessions` listing before and after, the `grep` result
  and the `llm_request` line.

## Gotchas

- `$V ab $R set offline on` does not close an open WebSocket in Chromium, so
  the page stays `idle`. Close the wrapped socket instead. It runs the
  client's real `onclose` path; it does not prove a server-side or network
  drop.
- Wrap `WebSocket` after the last `open` or `reload` and before
  `New private conversation`; a reload or `record start` discards the wrapper.
- Reloading a private conversation does not show the ended notice. The
  reload never asks for a private session, so it opens an ordinary one.
- Private sessions record metrics whatever `THALVI_RECORD` says. The fake run
  does not set text or audio recording, so that override is not exercised.
- Learnings retirement and background titling are off in the fake run, so
  their exclusion of private turns is not exercised.
