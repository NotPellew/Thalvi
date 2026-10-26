# AGENTS.md

Thalvi is a voice assistant for when hands and eyes are busy: cooking,
driving, carrying things. The goal is an assistant its developer enjoys using
every day; a feature that works but feels clumsy is not done. Decisions in
force live in [DECISIONS.md](DECISIONS.md); read the areas your change touches
before writing code.

Here, *the user* is the person talking to Thalvi, and *the developer* is the
person directing you.

## Non-negotiables

- Thalvi never sends a message or takes any other action that leaves the
  device on the user's behalf without reading it back and hearing a spoken
  yes.
- Never rewrite pushed history, on `main` or on a PR branch: no force-push, no
  rebase or squash of pushed commits. Bring `main` into a branch with a merge.
- Every commit message and PR title follows Conventional Commits:
  `type(scope): summary`, the summary starting lowercase. Types: `feat fix docs
  refactor test chore ci build perf`. Scopes: `server web ci deps docs`. CI
  checks both. Run `git config core.hooksPath .githooks` once per clone.
- Never commit secrets, model weights, datasets, audio or camera captures,
  logs, plans or scratch notes, or PR screenshots. A committed secret is
  rotated; deleting the file does not fix it.
- Tests and CI never call Meta's model APIs or any other paid service.
- If a rule here fights the task, say so and get the developer's sign-off
  before breaking it.

## What makes Thalvi good to use

- **Voice first.** Every flow works by voice alone. The screen helps, but no
  flow requires looking at it.
- **Fast.** The user feels the gap between their last word and Thalvi's first
  sound. Stream transcription, reasoning, and speech; add nothing that blocks
  that path without knowing what it costs.
- **Interruptible.** The user can cut in at any moment; Thalvi stops talking
  at once and listens.
- **Talks like a person.** Short spoken sentences, one step at a time. Text
  written for a screen (lists, markdown, URLs) is never read aloud. When
  unsure what the user meant, ask briefly instead of guessing.
- **Never goes silent.** When something fails, Thalvi says so in plain words
  and the conversation carries on.
- **Fewer, finished features.** Finish one flow from first word to last before
  starting the next.

## Scope

- Do what the developer asked, nothing more. A refactor, cleanup, or feature
  you notice on the way is mentioned, not done.
- Before adding complexity for an unlikely case (a race, an attack, scale we
  don't have), state the failure, how likely it is, and the cost, and ask.

## Do not over-engineer

Every layer that doesn't make Thalvi better to use is time not spent on one
that does. Build the smallest thing that does the job for the cases we have.

- No speculative generality: no trait or interface with one implementation, no
  option nobody sets, no plugin point, no layer "for later". Add the
  abstraction when the second implementation arrives. A test fake counts,
  which is why model APIs sit behind an interface.
- Don't extract a single-use helper unless it names a real concept.
- Validate external input once, where it enters (network, microphone, camera,
  model output, config), and trust the typed value inside.
- No compatibility shims or dead paths: when something changes, update its
  callers and delete the old code in the same change.
- Prefer the standard library and existing dependencies. A new dependency
  needs a reason in the PR.

## Code quality

- Comments: none inline, in tests too. Rust `///` states an item's contract
  and any non-obvious reason; every module starts with a `//!` line saying
  what it owns. A comment that restates the code is deleted.
- Types carry the rules: make illegal states unrepresentable. No `any` in
  TypeScript; no `unwrap`/`expect` outside tests in Rust.
- Rust: `crates/` is flat, directory name = crate name. Dependency versions
  and lints are set at the workspace root. `lib.rs` only declares and
  re-exports modules. No `mod.rs`.
- Prefer a lint or a CI check over a new rule in this file.
- Fix root causes: trace a bug to where it starts. A guard is a fix only when
  the state it handles is legitimate.
- Tests check behavior a caller can observe, not internals. A bug fix adds a
  test that fails without it. A test that needs a sleep or timeout is wrong.
- Before calling work done, run the gates and use the change the way the user
  would: speech in, speech out, with real or recorded audio. Report what you
  ran.

## Gates

Nothing is scaffolded yet. The PR that adds a component adds its pinned
toolchain (`rust-toolchain.toml`, `.node-version`), its gate commands here,
and its CI job as a required check on `main`. npm is the package manager;
lockfiles are committed. CI runs exactly the commands listed here.

## Git and PRs

- Commit in small steps that each say what changed. PRs land as merge
  commits, so every commit stays in the history.
- Branches: `<type>/<slug>`, three hyphenated words or fewer.
- The PR title becomes the merge commit's subject. The body states the
  problem, then the fix. When an issue exists, say `Closes #N`.

## Docs

- Write only what the code cannot answer.
- A DECISIONS.md bullet records a choice someone might revisit:
  `**Name.** Rule. Why: … Rejected: …`, four lines at most. Edit it in place,
  in the PR that changes the decision.
- This file changes in the same PR as the convention it describes.
