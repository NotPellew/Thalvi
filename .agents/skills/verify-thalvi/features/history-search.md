# History search

When the model calls the read-only `search_history` tool, the user sees
Working while it runs unless active voice playback takes precedence. An
initially collapsed `Task steps` section records the search and its match
count, and the answer can draw on earlier and past conversations.

## Sub-features

- `search-tool` runs the model's `search_history` call and returns excerpts to
  the model as data.
- `search-working` shows the `working` state while the tool runs.
- `search-steps` adds a `Task steps` section, collapsed by default, with
  `Searching conversations`, its status and the match count.
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
  `Conversations` and `New conversation`, and run `$V state $R idle`. Reload
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
- **Proof.** Screenshot with the steps open, plus the two `llm_request` lines.

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
- After opening Task steps, another search currently leaves it open. The
  collapsed assertion above applies after page load. This differs from the
  DECISIONS.md rule that task steps stay collapsed; report it as a product
  discrepancy, not proof that every search starts collapsed.
