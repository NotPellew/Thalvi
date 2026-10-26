# AGENTS.md

Thalvi is a voice assistant for when hands and eyes are busy. The goals are
solid engineering, good UX, and a lean codebase. Decisions in force live in
[DECISIONS.md](DECISIONS.md); read the areas your change touches before
writing code.

Here, *the user* is the person using Thalvi, and *the developer* is the person
directing you.

## Non-negotiables

- Nothing leaves the device on the user's behalf, such as a message, until
  the user has confirmed it.
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

## Good UX

- Latency our code adds is a bug. Keep the path from input to response short,
  use results as they stream in instead of waiting for whole responses, and
  move work that can wait off that path. Model and network latency are outside
  this project's control; don't build machinery to hide them.
- The user can always tell what Thalvi is doing, and can interrupt or cancel
  it at any moment.
- When something fails, the user is told what happened in plain words and the
  session recovers. No silent failures, no stuck states.
- Finish one flow end to end before starting the next. Fewer polished
  features beat many rough ones.

## Solid engineering

- Types carry the rules: make illegal states unrepresentable. No `any` in
  TypeScript; no `unwrap`/`expect` outside tests in Rust.
- Validate external input once, where it enters (network, devices, model
  output, config), and trust the typed value inside.
- Fix root causes: trace a bug to where it starts. A guard is a fix only when
  the state it handles is legitimate.
- Tests: full coverage, no bloat. Every behavior is tested, with as few tests
  as that takes: one test per behavior, not per function, and table-driven
  cases instead of near-copies. Test what a caller can observe, not
  internals, and only failures that can really happen, not invented or
  far-fetched ones. A bug fix adds a test that fails without it. Tests never
  wait on the wall clock; use a paused or fake clock.
- Comments: none inline, in tests too. Rust `///` states an item's contract
  and any non-obvious reason; every module starts with a `//!` line saying
  what it owns. A comment that restates the code is deleted.
- Rust: `crates/` is flat, directory name = crate name. Dependency versions
  and lints are set at the workspace root. `lib.rs` only declares and
  re-exports modules. No `mod.rs`.
- Prefer a lint or a CI check over a new rule in this file.
- Before calling work done, run the gates and use the change end to end the
  way the user would. Report what you ran.

## Keep it lean (YAGNI)

Build what is needed now, for the cases we have. Code that isn't written
can't break and doesn't slow down the next change.

- Do what the developer asked, nothing more. A refactor, cleanup, or feature
  you notice on the way is mentioned, not done.
- No speculative generality: no trait or interface with one implementation, no
  option nobody sets, no plugin point, no layer "for later". Add the
  abstraction when the second implementation arrives. A test fake counts,
  which is why model APIs sit behind an interface.
- Don't extract a single-use helper unless it names a real concept.
- No compatibility shims or dead paths: when something changes, update its
  callers and delete the old code in the same change.
- Prefer the standard library and existing dependencies. A new dependency
  needs a reason in the PR.
- Before adding complexity for an unlikely case (a race, an attack, scale we
  don't have), state the failure, how likely it is, and the cost, and ask.

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
