# Conversations and learnings

Conversations with turns are saved; empty ones are not listed. A reload
rejoins the same one. The Conversations sheet lists saved conversations and lets the user start a new one, reopen,
rename, or delete one after confirming. Its Learnings section shows durable
facts the user can read and correct.

## Sub-features

- `conv-rejoin` brings back the same conversation after a reload.
- `conv-list` lists saved conversations, newest first, with age.
- `conv-new` starts an empty conversation and closes the sheet.
- `conv-reopen` opens a saved conversation with its turns.
- `conv-rename` renames a conversation; the title persists.
- `conv-delete` asks `Delete “<title>”? This can't be undone.`, offers
  `Keep it`, and deletes only on `Delete`. Deleting the open conversation
  starts an empty new one.
- `learn-list` shows stored learnings under `Learnings`.
- `learn-edit` edits a learning and persists it.

## How to get to it (user POV)

- Reload the page.
- Choose the list icon (`Conversations`) in the header.
- In the sheet: `New conversation`, a row's title, `Rename`, `Delete`, and the
  `Learnings` toggle followed by `Edit`.

## Driving it with thalvi-verify

Preconditions:

- Baseline from `features/README.md`, page at `idle`.
- Start a new conversation through `Conversations` → `New conversation`,
  wait for `idle`, then send `What should I cook with leftover rice`.
  Run `$V reply $R --after "What should I cook with leftover rice"`.
  This seed is required even when other feature recipes ran first.

Steps:

- **Rejoin.** Reload the page.
  - Run `$V ab $R reload` and `$V state $R idle`.
  - `.o-turns` holds the same turns as before.
- **List.** Open the sheet.
  - Run `$V ab $R click 'button[aria-label="Conversations"]'`,
    `$V ab $R wait 'button[aria-label="Delete What should I cook with leftover rice"]'`
    and `$V ab $R snapshot -i -c`.
  - A row button reads `What should I cook with leftover rice just now`,
    with `Rename …` and `Delete …` beside it.
  - `curl <URL>sessions` lists the same id and title.
- **New.** Start a new conversation.
  - Run `$V ab $R find role button click --name "New conversation"` and
    `$V state $R idle`.
  - The sheet closes and `.o-turns` is empty.
- **Second conversation.** Send `A second conversation` and run
  `$V reply $R --after "A second conversation"`.
  - Reopen the sheet; it now has rows for both new conversations, plus any earlier ones.
- **Rename.** Choose `Rename What should I cook with leftover rice`, type
  `Rice ideas` and press Enter.
  - The sheet is already open. Wait for the row as in **List**, then run
    `$V ab $R click 'button[aria-label="Rename What should I cook with leftover rice"]'`,
    `$V ab $R fill 'input[aria-label="Conversation title"]' 'Rice ideas'` and
    `$V ab $R press Enter`.
  - `button[aria-label="Rename Rice ideas"]` appears, and `curl <URL>sessions`
    shows the title `Rice ideas`.
  - `document.activeElement.getAttribute("aria-label")` reads `Rename Rice ideas`.
- **Reopen.** Choose the `Rice ideas` row.
  - Run `$V ab $R find role button click --name "Rice ideas"` and
    `$V state $R idle`.
  - The sheet closes and `.o-turns` starts with `What should I cook with
    leftover rice`.
- **Delete, keep.** Open the sheet, wait for
  `button[aria-label="Delete Rice ideas"]`, and choose it.
  - `.o-lib-ask` reads `Delete “Rice ideas”? This can't be undone.`
  - Choose `Keep it` (`find role button click --name "Keep it"`); the row
    stays.
- **Delete.** Choose `Delete Rice ideas`, then the confirming `Delete`.
  - Run `$V ab $R click 'button[aria-label="Delete Rice ideas"]'` and
    `$V ab $R click '.o-lib-danger'`.
  - The row disappears and the sheet stays open.
  - Focus moves to the next row's main button (or `New conversation` when
    there are no rows). Capture `document.activeElement.outerHTML`.
  - That session's file is gone from `R/data/sessions/`.
  - Because `Rice ideas` was open, `.o-turns` is now empty, a new conversation
    in state `idle`.
- **Learnings.** Choose `Learnings`, then `Edit` on the seeded item.
  - Run `$V ab $R find role button click --name "Learnings"`,
    `$V ab $R wait 'button[aria-label^="Edit: Prefers metric"]'`, then
    `$V ab $R click 'button[aria-label^="Edit: Prefers metric"]'`.
  - Then run `$V ab $R fill 'textarea[aria-label="What was learned"]' 'Prefers metric units and gram weights.'`
    and `$V ab $R find role button click --name "Save"`.
  - `button[aria-label^="Edit: Prefers metric units and gram"]` appears, and
    `R/data/learnings.json` holds the new text.
- **Proof.** Screenshot and ARIA-snapshot the sheet after the rename and after
  the delete. Keep `curl <URL>sessions` output and the `R/data/` files.

## Gotchas

- Fallback titles use the first nonblank user line, normalize whitespace and
  truncate at 40 characters. The fake run sets no `BG_LLM_*`,
  so background titling is not exercised. Claim it only with a background-model
  stand-in.
- Reopening the same saved session in another tab takes it over: the old tab
  ends before deletion. For a deletion-notice check, leave the first tab
  attached and delete via a second client's list without reopening that session.
  This cross-client path is not exercised by the single-tab recipe.
- Row labels include the age (`just now`, `1 min ago`). Match the
  `aria-label` of `Rename …` and `Delete …`, which carry only the title.
- While the sheet is open the rest of the page is `inert`. Close it
  (`find role button click --name "Close"`) before using the composer.
- Learnings come only from the seed file or from retirement
  (`THALVI_RETIRE_DAYS`), which the fake run keeps off. Retirement itself is not
  covered.
- A blank learning shows a validation message. Normal UI typing is capped at
  1000 characters by the textarea; overlength API rejection is a separate
  boundary check, not a normal UI journey.
