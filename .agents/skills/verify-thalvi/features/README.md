# Thalvi verification map

This is the source for verifying Thalvi's user-facing web behavior. Read the
index before driving the app, then use the matching feature file as the recipe.

## Baseline preconditions

- A fake-mode run started with
  `.agents/skills/verify-thalvi/scripts/thalvi-verify up` from the repository
  root. The scripted replies these recipes assert exist only in fake mode. With
  `up --local`, follow the same steps but assert the shape of each result, as
  `SKILL.md` describes. `R` is the `RUN_DIR` it printed, and
  `V=.agents/skills/verify-thalvi/scripts/thalvi-verify`.
- `$V doctor $R` prints only `ok` lines.
- The page is open: `$V ab $R open "$(. $R/run.env; echo $URL)"`, then
  `$V state $R idle`.
- Seed data: no conversations; one learning, `Prefers metric units in
  recipes.`; voices `fake_heart` (default) and `fake_bella`; an empty
  recognizer queue.
- Never drive an instance this task's `up` did not start.

## Driving conventions

- Run recipes in the index order below, or independently. Each recipe names
  its own preconditions. Conversations creates its rice seed in a new
  conversation; history search creates a separate seed even if rice was deleted.
  Voice picker needs untouched browser preferences; use a fresh run if needed.
- Prefer accessible names and stable classes (see the handle table in
  `SKILL.md`) over coordinates or refs from an older snapshot.
- After sending or speaking a turn, wait with
  `$V reply $R --after "<unique user text>"`, then assert its expected answer.
  A phrase already in an older reply can pass before the new turn is echoed.
  Never wait with `$V state $R idle`, which passes before the server answers. Use
  `state` only for a state the page is not in yet. Never sleep for a fixed
  time.
- After `open`, `reload`, `New conversation` or reopening, run
  `$V state $R idle` before typing. While the page is `connecting`, a fill and
  click report success but nothing is sent.
- Queue every spoken turn with `$V say $R "<words>"` before voice is on, or
  while the previous turn is being answered.
- Turn voice off when a recipe ends. The looping microphone otherwise keeps
  feeding the recognizer.

## Proof and skip reporting

- UI proof: a screenshot and an ARIA snapshot in `R/evidence/`, named
  `<feature>-<step>`. Record a `.webm` for flows that unfold over time (speaking,
  stopping, barge-in). Start the recording with the URL before the first turn
  (see `SKILL.md`, Evidence).
- Side-effect proof: the matching lines of `R/fakes.jsonl`, recording metrics
  and the stored file under `R/data/`. Cancellation metrics are authoritative;
  fake cancellation logs are conditional on another stream write.
- Report a path you could not reach with the command you tried and the
  precondition that was missing. Never report one entry point as verified
  through another.
- Fakes stand in for every model. Do not report real model, recognizer or
  audio quality as verified.

## Feature entry contract

Each feature file starts with an H1 title and one paragraph describing the
user-visible behavior. It then has exactly four H2 sections, in this order:

- `Sub-features`
- `How to get to it (user POV)`
- `Driving it with thalvi-verify`
- `Gotchas`

## Features

- [Typed conversation](./typed-conversation.md) covers typed turns, streamed
  replies, stopping, and the failure notice.
- [Voice conversation](./voice-conversation.md) covers voice on and off, live
  captions, spoken replies, stopping, and barge-in.
- [Conversations and learnings](./conversations.md) covers rejoin on reload,
  the conversation list, new, reopen, rename, delete, and learnings.
- [History search](./history-search.md) covers the `search_history` tool, the
  Working state, collapsed Task steps, and cross-conversation matches.
- [Voice picker and speed](./voice-and-speed.md) covers previewing and applying
  a voice, and playback speed.

Desktop features are outside this web map. The overlay, Settings window and
looks (`crates/thalvi-app/ui/src/look.ts`), model download notices and tray
commands are driven in a sandboxed nested Hyprland by
[desktop.md](../desktop.md). Dictation by voice
(`crates/thalvi-desktop/src/local.rs`) needs a microphone, which that sandbox
does not have; neither recipe verifies it.
