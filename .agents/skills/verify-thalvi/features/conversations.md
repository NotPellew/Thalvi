# Conversations and learnings

Ordinary conversations with turns are saved; empty ones are not listed. A
reload rejoins the same one. The Conversations sheet lists saved conversations
and lets the user start a new one, reopen, rename, or delete one after
confirming. Private conversations are never saved; see
[private-conversation.md](./private-conversation.md). Its Learnings section shows durable
facts the user can read and correct.

## Sub-features

- `conv-rejoin` brings back the same conversation after a reload.
- `conv-list` lists saved conversations, most recently updated first, with
  age.
- `conv-new` starts an empty conversation and closes the sheet.
- `conv-preview` shows a saved conversation's turns without joining it, and
  warns when another device has it open.
- `conv-reopen` joins the previewed conversation through `Continue here`.
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
- Start a new conversation: `$V ab $R click 'button[aria-label="Conversations"]'`,
  `$V ab $R click 'button.o-lib-new'` and `$V state $R idle`, then send `What should I cook with leftover rice`.
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
  - Run `$V ab $R click 'button.o-lib-new'` and
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
- **Preview.** Choose the `Rice ideas` row.
  - Run `$V ab $R find role button click --name "Rice ideas"` and
    `$V ab $R wait --fn 'document.querySelector(".o-library")?.innerText.includes("Continue here")'`.
  - The sheet shows the title and the turns labelled `You` and `Thalvi`; the
    page's own `.o-turns` is unchanged.
- **Reopen.** Choose `Continue here`.
  - Run `$V ab $R find role button click --name "Continue here"` and
    `$V ab $R wait --fn 'document.querySelector(".o-turns").innerText.includes("What should I cook")'`.
    `$V state $R idle` alone can pass before the page switches over.
  - The sheet closes and `.o-turns` starts with `What should I cook with
    leftover rice`.
  - A second browser that has the conversation open shows `This conversation
    continued on another device.` and stays stopped. Drive it with
    `agent-browser --session <name>`; previewing from it never stops the first.
- **Delete, keep.** Open the sheet, wait for
  `button[aria-label="Delete Rice ideas"]`, and choose it.
  - `.o-lib-ask` reads `Delete “Rice ideas”? This can't be undone.`
  - Choose `Keep it` (`find role button click --name "Keep it"`); the row
    stays.
- **Delete.** Choose `Delete Rice ideas`, then the confirming `Delete`.
  - Run `$V ab $R click 'button[aria-label="Delete Rice ideas"]'`,
    `$V ab $R click '.o-lib-danger'` and
    `$V ab $R wait --fn '!document.querySelector(".o-lib-ask") && !document.querySelector("button[aria-label=\"Delete Rice ideas\"]")'`.
    The question also closes when deletion fails; the row staying is that
    failure.
  - The row disappears and the sheet stays open.
  - Focus moves to the main button of the row that took the deleted row's
    place, or to `New conversation` when the deleted row was the last.
    Capture `document.activeElement.outerHTML`.
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

- Fallback titles use the first nonblank line of the first user turn,
  normalize whitespace, and cut a longer line to 40 characters plus `…`. The fake run sets no `BG_LLM_*`,
  so background titling is not exercised. Claim it only with a background-model
  stand-in.
- Reopening the same saved session in another tab takes it over: the old tab
  ends before deletion. For a deletion-notice check, leave the first tab
  attached and delete via a second client's list without reopening that session.
  This cross-client path is not exercised by the single-tab recipe.
- Row labels include the age (`just now`, `1 min ago`). Match the
  `aria-label` of `Rename …` and `Delete …`, which carry only the title.
  Earlier recipes leave rows with the same rice title; a selector picks the
  first row, the most recently updated one, which is this recipe's seed as
  long as nothing else touched an older rice conversation since.
- Use `button.o-lib-new` for the ordinary path. The sheet also has
  `New private conversation`, so a name-based `find` is ambiguous.
- While the delete question shows, the row's `Delete …` button is gone, so
  waiting for it to vanish passes before the deletion. Wait for `.o-lib-ask`
  to go instead.
- While the sheet is open the rest of the page is `inert`. Close it
  (`find role button click --name "Close"`) before using the composer.
- Learnings come only from the seed file or from retirement
  (`THALVI_RETIRE_DAYS`), which the fake run keeps off. Retirement itself is not
  covered.
- A blank learning shows a validation message. Normal UI typing is capped at
  1000 characters by the textarea; overlength API rejection is a separate
  boundary check, not a normal UI journey.
