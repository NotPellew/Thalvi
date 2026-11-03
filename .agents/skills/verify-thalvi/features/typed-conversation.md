# Typed conversation

The user types a message, sees Thalvi think and stream a reply into the
conversation, can stop a reply mid-way, and is told in plain words when the
model fails, after which the session keeps working.

## Sub-features

- `typed-send` sends the composer text as a user turn and clears the composer.
- `typed-reply` streams the reply into a Thalvi turn and returns to Idle.
- `typed-stop` stops a reply in progress from the reticle.
- `typed-failure` shows a plain-words notice when the model fails, then
  recovers.

## How to get to it (user POV)

- Type in the `Message` box and press Enter, or choose `Send`.
- While Thalvi is thinking or speaking, tap the reticle (`Stop`).

## Driving it with thalvi-verify

Preconditions:

- Baseline from `features/README.md`, page at `idle`, voice off.

Steps:

- **Send.** Type a question and choose `Send`.
  - Run `$V ab $R fill '#o-message' 'How do I proof bread dough?'` and
    `$V ab $R click 'button[aria-label="Send"]'`.
  - `#o-message` is empty at once.
- **Reply.** Wait for the answer.
  - Run `$V reply $R --after "How do I proof bread dough?"`.
  - It prints the last Thalvi turn,
    `Fake reply to: How do I proof bread dough?. That is all.`, and the state
    is `idle`.
  - The `.o-turns > .o-user` turn before it reads `How do I proof bread
    dough?`. It appears only once the server echoes it.
  - `R/fakes.jsonl` has an `llm_request` with that `user` text and
    `"tools": ["search_history"]`.
  - With voice off there is no `tts_request`.
- **Stop.** Send `Tell me a long story` and stop it while it streams.
  - Run the same `fill` and `click`, then wait for visible streamed text:
    `$V ab $R wait --fn 'document.querySelector(".orb-app").dataset.state === "thinking" && document.querySelector(".o-turns > .o-assistant:last-child")?.textContent.includes("This is a long fake answer")'`.
    Run `$V ab $R click 'button[aria-label="Stop"]'` and `$V state $R idle`.
  - The state returns to `idle`, and the Thalvi turn holds only the text
    streamed so far.
  - The recording gains `interrupt` with `reason: cancel` and an interrupted
    reply. `llm_cancelled` may follow if a fake stream write sees the close.
- **Failure.** Send `Please make it fail`.
  - Run `$V ab $R wait '.o-notice:not([hidden])'` and
    `$V ab $R get text '.o-notice'`.
  - The notice reads `The language model could not reply: the language model
    returned an error (500 Internal Server Error)`, and the state returns to
    `idle`.
- **Recovery.** Send `Are you still there?`.
  - `$V reply $R --after "Are you still there?"` passes, and `.o-notice` is hidden
    again.
- **Proof.** Capture the result.
  - Run `$V ab $R screenshot $R/evidence/typed-final.png` and
    `$V ab $R snapshot -c > $R/evidence/typed-final.aria.txt`.
  - Save the matching `fakes.jsonl` lines next to them.

## Gotchas

- Any message containing `make it fail`, `long story` or `search for <words>`
  takes the fake's scripted path. `How long …` is an ordinary echo.
- Reply text appears while the state is still `thinking`, and `idle` holds
  before the server answers. Use `reply --after` with a unique prompt to wait
  for this answer. Sending again earlier interrupts the open reply.
- `Send` is disabled while the composer is empty or the socket is offline.
- The notice clears on the next turn. Capture it before sending again.
