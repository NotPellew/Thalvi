# AGENTS.md

Thalvi is a hands-free voice assistant, built in nine days for a hackathon.
Decisions in force live in [DECISIONS.md](DECISIONS.md); read the areas your
change touches before writing code.

## Non-negotiables

- Never rewrite pushed history, on `main` or on a PR branch: no force-push, no
  rebase or squash of pushed commits. Bring `main` into a branch with a merge.
- Every commit message and PR title follows Conventional Commits:
  `type(scope): summary`, the summary starting lowercase. Types: `feat fix docs
  refactor test chore ci build perf`. Scopes: `server web ci deps docs`. CI
  checks both. Run `git config core.hooksPath .githooks` once per clone.
- Follow the [hackathon provenance rules](README.md#hackathon-provenance).
- Never commit secrets, model weights, datasets, audio or camera captures,
  logs, plans or scratch notes, or PR screenshots. A committed secret is
  rotated; deleting the file does not fix it.
- Tests and CI never call Meta's model APIs or any other paid service.
- If a rule here fights the task, say so and get sign-off before breaking it.

## Scope

- Do what was asked, nothing more. A refactor, cleanup, or feature you notice
  on the way is mentioned, not done.
- Before adding complexity for an unlikely case (a race, an attack, scale we
  don't have), state the failure, how likely it is, and the cost, and ask.

## Do not over-engineer

The hackathon is judged on a working demo, and every extra layer is time not
spent on it. Build the smallest thing that does the job for the cases we have.

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
- Before calling work done, run the gates and exercise the changed behavior
  for real. Report what you ran.

## Gates

Nothing is scaffolded yet. The PR that adds a component adds its pinned
toolchain (`rust-toolchain.toml`, `.node-version`), its gate commands here,
and its CI job as a required check on `main`. npm is the package manager;
lockfiles are committed. CI runs exactly the commands listed here.

## Git and PRs

- Commit in small steps that each say what changed; PRs land as merge
  commits, so every commit is part of the record.
- Branches: `<type>/<slug>`, three hyphenated words or fewer.
- The PR title becomes the merge commit's subject. The body states the
  problem, then the fix. When an issue exists, say `Closes #N`.

## Docs

- Write only what the code cannot answer.
- A DECISIONS.md bullet records a choice someone might revisit:
  `**Name.** Rule. Why: … Rejected: …`, four lines at most. Edit it in place,
  in the PR that changes the decision.
- This file changes in the same PR as the convention it describes.
