# Conversations and learnings

Every conversation is saved. A reload rejoins the same one. The Conversations
sheet lists saved conversations and lets the user start a new one, reopen,
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
- At least one typed turn sent in this run, for example
  `What should I cook with leftover rice` (see `typed-conversation.md`),
  waited for with `$V reply $R "That is all."`.

Steps:

- **Rejoin.** Reload the page.
  - Run `$V ab $R reload` and `$V state $R idle`.
  - `.o-turns` holds the same turns as before.
- **List.** Open the sheet.
  - Run `$V ab $R click 'button[aria-label="Conversations"]'`,
    `$V ab $R wait 'button[aria-label="Delete What should I cook with leftover rice"]'`
    and `$V ab $R snapshot -i -c`.
  - One row button reads `What should I cook with leftover rice just now`,
    with `Rename …` and `Delete …` beside it.
  - `curl <URL>sessions` lists the same id and title.
- **New.** Start a new conversation.
  - Run `$V ab $R find role button click --name "New conversation"` and
    `$V state $R idle`.
  - The sheet closes and `.o-turns` is empty.
- **Second conversation.** Send `A second conversation` and run
  `$V reply $R "A second conversation"`.
  - Reopen the sheet; it now has two rows.
- **Rename.** Choose `Rename What should I cook with leftover rice`, type
  `Rice ideas` and press Enter.
  - Open the sheet and wait for the row as in **List**, then run
    `$V ab $R click 'button[aria-label="Rename What should I cook with leftover rice"]'`,
    `$V ab $R fill 'input[aria-label="Conversation title"]' 'Rice ideas'` and
    `$V ab $R press Enter`.
  - `button[aria-label="Rename Rice ideas"]` appears, and `curl <URL>sessions`
    shows the title `Rice ideas`.
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
  - That session's file is gone from `R/data/sessions/`.
  - Because `Rice ideas` was open, `.o-turns` is now empty, a new conversation
    in state `idle`.
- **Learnings.** Choose `Learnings`, then `Edit` on the seeded item.
  - Run `$V ab $R find role button click --name "Learnings"` and
    `$V ab $R click 'button[aria-label^="Edit: Prefers metric"]'`.
  - Then run `$V ab $R fill 'textarea[aria-label="What was learned"]' 'Prefers metric units and gram weights.'`
    and `$V ab $R find role button click --name "Save"`.
  - `button[aria-label^="Edit: Prefers metric units and gram"]` appears, and
    `R/data/learnings.json` holds the new text.
- **Proof.** Screenshot and ARIA-snapshot the sheet after the rename and after
  the delete. Keep `curl <URL>sessions` output and the `R/data/` files.

## Gotchas

- Titles are the first line typed or spoken. The fake run sets no `BG_LLM_*`,
  so background titling is not exercised. Claim it only with a background-model
  stand-in.
- Another browser tab that had the deleted conversation open shows the
  `ended` state with a notice. One tab cannot show this; a second
  agent-browser tab can.
- Row labels include the age (`just now`, `1 min ago`). Match the
  `aria-label` of `Rename …` and `Delete …`, which carry only the title.
- While the sheet is open the rest of the page is `inert`. Close it
  (`find role button click --name "Close"`) before using the composer.
- Learnings come only from the seed file or from retirement
  (`THALVI_RETIRE_DAYS`), which the fake run keeps off. Retirement itself is not
  covered.
- Saving a learning that is blank or over 1000 characters is refused with a
  plain message. Assert the message, not a saved value.
