# AGENTS.md

Thalvi is a voice assistant for when hands and eyes are busy. The goals are
solid engineering, good UX, and a lean codebase. Decisions in force live in
[DECISIONS.md](DECISIONS.md); read the areas your change touches before
writing code.

Here, *the user* is the person using Thalvi, and *the developer* is the person
directing you.

## Non-negotiables

- Thalvi asks the user to confirm right before any action that reaches other
  people or can't be undone, such as sending a message.
- Never rewrite `main`'s history.
- Every commit message and PR title follows Conventional Commits:
  `type(scope): summary`, the summary not starting with a capital letter.
  Types: `feat fix docs refactor test chore ci build perf`. Scopes: `server web
  desktop ci deps docs`. The commit-msg hook checks this, and CI runs it on
  every commit and the PR title. Run `git config core.hooksPath .githooks` once
  per clone.
- Never commit secrets, model weights, datasets, audio or camera captures,
  logs, plans or scratch notes, or PR screenshots. A committed secret is
  rotated; deleting the file does not fix it.
- Tests and CI never call Meta's model APIs or any other paid service.
- If a rule here fights the task, say so and get the developer's sign-off
  before breaking it.

## Good UX

- Latency our code adds is a bug. Keep the path from input to response short,
  use results as they stream in instead of waiting for whole responses, and
  move work that can wait off that path. Tricks that make waiting on a model
  feel shorter (starting early, a quick acknowledgement) are welcome when the
  recordings show they help, but numbers stay honest: report a model's real
  latency and the latency the user perceives separately, never one as the
  other.
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
- Every call to a model, endpoint or tool records its latency and outcome in
  the opt-in metrics, so changes can be judged on real use.
- Fix root causes: trace a bug to where it starts. A guard is a fix only when
  the state it handles is legitimate.
- Tests: full coverage, no bloat. Every behavior is tested, with as few tests
  as that takes: one test per behavior, not per function, and table-driven
  cases instead of near-copies. Test what a caller can observe, not
  internals, and only failures that can really happen, not invented or
  far-fetched ones. A bug fix adds a test that fails without it. Never sleep
  for a fixed time: drive time with a paused or fake clock, or wait for an
  observable event under a bounded timeout.
- Comments: default to none. Write one only for what the code can't say: a
  constraint, a platform quirk, an invariant (`// SAFETY:` on every `unsafe`
  block). Keep it to one line where possible. Never restate the code or
  narrate history; the commit carries that. Rust `///` states an item's
  contract, and every module starts with a `//!` line saying what it owns,
  so `grep -rm1 '^//!'` maps the code.
- Desktop OS code lives in `#[cfg(target_os)]` modules behind the desktop host
  interface; shared logic has no `cfg`. Every host implements every method; what
  a desktop can't do returns a typed reason the user sees, never a no-op.
- Rust: `crates/` is flat, directory name = crate name. Dependency versions
  and lints are set at the workspace root. `lib.rs` only declares and
  re-exports modules. No `mod.rs`.
- Prefer a lint or a CI check over a new rule in this file.
- Before calling work done, run the gates and use the change end to end the
  way the user would. Report what you ran and what you could not verify.
  The `verify-thalvi` skill (`.agents/skills/verify-thalvi/`) runs the web
  client on fake or self-hosted endpoints for this. A desktop PR names each
  desktop it was used on; CI and fixtures are not use.

## Keep it lean (YAGNI)

Build what is needed now, for the cases we have. Code that isn't written
can't break and doesn't slow down the next change.

- Keep changes to what the developer asked. Mention unrelated refactors,
  cleanups, or features you notice instead of doing them.
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

CI runs exactly these as required checks on `main`. A new component adds its
pinned toolchain, its commands here, and its CI job; one that ships on Windows
also runs its gate on `windows-latest`. npm is the package manager; lockfiles
are committed.

- Server (`crates/`): `cargo fmt --all -- --check`, `cargo lint`,
  `cargo test --workspace --locked`, `RUSTDOCFLAGS="-D warnings" cargo docs`.
- Web (`web/`): `npm ci`, `npm run typecheck`, `npm run lint`, `npm test`,
  `npm run build`.
- Desktop (`desktop/`): Python is pinned in `.python-version`, uv is pinned in
  CI, and emitted browser functions run on Node pinned in the root `.node-version`.
  Run `uv sync --locked`, `uv run ruff format --check .`,
  `uv run ruff check .`, `uv run pytest -q`. This gate uses fakes instead of
  model weights, microphones, or real desktop apps.
- Desktop browser (`desktop/browser/`): from `desktop/`, `npm ci --prefix browser`,
  `npm run build --prefix browser`, `uv run playwright install --with-deps chromium`,
  then `THALVI_BROWSER_TESTS=1 uv run pytest -q tests/test_discord_browser.py`.
  Runs the real Chromium and Slate fixture through a local fake `discord.com`;
  it never reaches real Discord.
- Speech servers (`compose.yaml`, `speech/`): `docker compose --profile kokoro
  --profile vllm-omni config -q`, `pipx run shellcheck-py==0.11.0.1
  speech/qwentts.sh`.

## Git and PRs

- Commit in small steps that each say what changed. PRs land as merge
  commits, so a branch's commits stay in the history. Tidying a branch
  before merge is fine; don't squash it into one commit.
- Branches: `<type>/<slug>`, three hyphenated words or fewer.
- The PR title becomes the merge commit's subject. The body states the
  problem, then the fix. Say `Closes #N` when the PR resolves an issue,
  `Refs #N` when it only touches one.
- Use `.github/pull_request_template.md` for every PR body, including PRs
  created through CLI tools; preserve its headings and remove instructional
  comments.
- Label every issue `area:desktop` or `area:conversation`, plus `windows`
  when Windows behaviour is involved.
- When opening an issue, use the matching form in `.github/ISSUE_TEMPLATE/`.
  For CLI-created issues, use its field labels as Markdown headings. Blank
  issues remain available for cases neither form fits; issues are optional.

## Docs

- Write only what the code cannot answer.
- A DECISIONS.md bullet records a choice someone might revisit:
  `**Name.** Rule. Why: … Rejected: …`, four lines at most. Edit it in place,
  in the PR that changes the decision.
- This file changes in the same PR as the convention it describes.
