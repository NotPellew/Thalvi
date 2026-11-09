# History search

When the model calls the read-only `search_history` tool, the user sees
Working while it runs unless active voice playback takes precedence. An
initially collapsed `Task steps` section records the search and its match
count, and the answer can draw on earlier and past conversations. In a
private conversation the search covers only that conversation's own turns.

## Sub-features

- `search-tool` runs the model's `search_history` call and returns excerpts to
  the model as data.
- `search-working` shows the `working` state while the tool runs.
- `search-steps` adds a `Task steps` section, collapsed by default, with
  `Searching conversations`, its status and the match count. Each new user
  turn clears it, so it starts collapsed again or hides when nothing ran.
- `search-past` finds matches in other saved conversations, not only the
  current one.

## How to get to it (user POV)

- Ask something that needs earlier context, typed or spoken. With the fake
  model, include `search for <words>`.
- Tap `Task steps` to open the step list.

## Driving it with thalvi-verify

Preconditions:

- Baseline from `features/README.md`, page at `idle`.
- One saved conversation that contains `leftover rice`. Create it by sending
  `What should I cook with leftover rice` and running
  `$V reply $R --after "What should I cook with leftover rice"`. Then choose
  `Conversations` and `button.o-lib-new`, and run `$V state $R idle`. Reload
  and wait for `idle` so Task steps starts with its initial collapsed state.

Steps:

- **Search.** In the new conversation, send `Search for leftover rice`.
  - Run `$V ab $R fill '#o-message' 'Search for leftover rice'` and
    `$V ab $R click 'button[aria-label="Send"]'`.
  - Then run `$V reply $R --after "Search for leftover rice"`.
  - It prints `Search done. The tool returned: {"count":…`.
- **Model side.** Check what reached the model.
  - `R/fakes.jsonl` has two `llm_request` lines for the turn.
  - The first has `"plan": "tool"`.
  - The second has a JSON-encoded string in `tool_results`; decode it. Its
    matches include a `session_id` other than the current conversation's,
    with the excerpt
    `What should I cook with leftover rice`.
- **Steps.** Check the step list.
  - `$V ab $R get text '.o-tasks summary'` reads `Task steps` and `1 step`.
  - `$V ab $R eval 'document.querySelector(".o-tasks").open'` is `false`.
  - Run `$V ab $R click '.o-tasks summary'`; `.o-task-list` reads
    `Searching conversations`, `DONE` and `<n> matches`.
- **Next turn.** With the steps still open, send
  `Search for leftover rice again` and run
  `$V reply $R --after "Search for leftover rice again"`.
  - `.o-tasks` is visible again with `1 step` and `open` is `false`.
  - This checks the reset, not a repeat match: the fake searches all three
    words, and `again` appears only in the current turn.
  - Send `Thanks for that` and wait with `reply --after`; `.o-tasks` is
    `hidden`.
- **Proof.** Screenshot and ARIA snapshot with the steps open, the two
  `llm_request` lines, and the `tool` metric in
  `R/data/recordings/<date>/<session>.jsonl` (`tool: search_history`,
  `outcome: ok`, `matches`).

## Gotchas

- `working` lasts only a few milliseconds with local data, so
  `$V state $R working` can miss it. Prove the tool ran from `Task steps` and
  `fakes.jsonl`, and record a `.webm` if the Working state itself is the claim.
- The fake only calls the tool when the request offers tools and the turn
  contains `search for`. A real model decides by itself, so this proves the
  tool path, not the model's choice to search.
- Matches require every query word in one turn. `search for rice leftovers`
  does not match `leftover rice`.
- The search also matches the current turn itself. Count the past
  conversation's `session_id`, not the total.
- The fake requests scope `all`; it does not exercise current-only searches or
  invalid tool arguments. The server caps matches at eight.
- `get text` returns the status as rendered (`DONE`); the DOM text is `Done`.
  A single result reads `1 match`.
