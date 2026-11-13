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
  client on fake or self-hosted endpoints for this, and its `desktop.md` runs
  the real desktop app in a sandboxed nested Hyprland. A desktop PR names each
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

CI runs these as required checks on `main`. `.github/scripts/gates <component>`
runs a component's checks exactly as CI does and stops at the first failure.
Components: `server`, `desktop-windows`, `web`, `desktop-ui`, `speech`,
`issue-sync`, `release`, `formal`; `all` runs every gate this OS can run and says which it skipped.
CI jobs call the script after their toolchain setup, so a new check goes in the
script, and a new component adds its pinned toolchain here, its case in the
script and its CI job; one that ships on Windows also runs its gate on
`windows-latest`. npm is the package manager; lockfiles are committed.

Prerequisites the script does not install:

- Server and Rust desktop (`crates/`): the Rust toolchain; on Linux
  `libasound2-dev libwebkit2gtk-4.1-dev libayatana-appindicator3-dev`; Node
  pinned in `.node-version`. `desktop-windows` needs the same toolchains on
  `windows-latest` and runs from Git Bash. CI then runs
  `.github/scripts/windows-smoke.ps1` on its installer: it installs and starts
  the app and opens Settings, and uploads screenshots.
- Web and desktop UI: Node pinned in `.node-version`.
- Speech servers: Docker with Compose, `pipx`, and `uv` (Python pinned in
  `speech/asr/.python-version`).
- Issue sync: `pipx`.
- Release (the updater signing and feed scripts): Node pinned in `.node-version`,
  `minisign`, and `pipx`.
- Formal source hashes: Python pinned in `.python-version` (standard library only).

Two required checks are not in the script:

- Dependencies (`deny.toml`, cargo-deny 0.20.2): `cargo deny --workspace --locked
  check advisories licenses sources`.
- Secrets (`.gitleaks.toml`, gitleaks 8.30.1): `gitleaks git --redact --no-banner
  --verbose --log-opts=HEAD .` on the full history. With gitleaks on `PATH`, the
  pre-commit hook scans what is staged.

Locally, run checks in three stages; none replaces a required CI result.

- **Editing:** the closest check that sees the edit: `cargo test -p <crate> <filter>`,
  or the web or desktop UI test and `npm run typecheck` for the files touched. No
  workspace tests, docs, mutation testing or packaging.
- **Before pushing,** with the work committed: `.github/scripts/changed origin/main HEAD`
  prints the affected units. Run each affected component's gate once:
  - Units `server`, `desktop`, `app`, `desktop-ui` (any of them): `TESTS="<packages>" BUNDLE=false
    .github/scripts/gates server`, where `<packages>` is `-p thalvi-server`, `-p thalvi-desktop` and
    `-p thalvi-app` for those of `server`, `desktop`, `app` printed, or `none`. `desktop-ui` also runs
    `.github/scripts/gates desktop-ui`.
  - Units `web`, `speech`: `.github/scripts/gates web`, `.github/scripts/gates speech`. Unit `issues`:
    `.github/scripts/gates issue-sync`.
  - `all` (an empty, unreadable or shallow diff, or any path outside the mapped ones, such as
    `crates/thalvi-app/tauri.conf.json`, its icons and build files): `.github/scripts/gates all`, unnarrowed.
    That is the only case that builds the AppImage or NSIS package locally.
  - A gate this OS can't run stays reported as skipped, never passed: off Windows, `desktop-windows`
    (units `desktop`, `app`, `desktop-ui`; on Windows its command is `TESTS="<-p thalvi-desktop and
    -p thalvi-app as printed, or none>" BUNDLE=false .github/scripts/gates desktop-windows`). Windows CI or a
    Windows machine covers it.
- **After pushing:** PR CI runs the affected tier, and the heavy run on `main` runs every gate.

On a PR, CI runs only the steps its changed paths affect
(`.github/scripts/changed`); the job names stay, so required checks still
report. `.github/workflows/heavy.yml` runs every gate on each push to `main`,
nightly and on request, plus mutation testing, which advises rather than gates:
`cargo mutants --in-place --in-diff <diff>` over the Rust changed since the
`verified` tag (cargo-mutants 27.1.0, `.cargo/mutants.toml`). Survivors are
listed in the run summary: add the missing test, or judge the mutant equivalent
and record that in `.cargo/mutants.toml` with a one-line reason. A shard that
couldn't test (failing baseline, time limit) or had a mutant time out fails the
run. A green full run
moves `verified`; a failed one opens or updates the `ci-regression` issue.
Run it locally only when the issue explicitly asks for mutation testing, and then
only `--in-diff` against the base, with no desktop session and a throwaway `HOME`,
since it runs tests against deliberately broken code.

## Git and PRs

- Commit in small steps that each say what changed. PRs land as merge
  commits, so a branch's commits stay in the history. Tidying a branch
  before merge is fine; don't squash it into one commit.
- Branches: `<type>/<slug>`, three hyphenated words or fewer, cut from
  `main`.
- A release bumps the version on `main` through a PR (`chore(desktop):
  release X.Y.Z`), then tags `desktop-vX.Y.Z` on `main`.
- The PR title becomes the merge commit's subject. The body states the
  problem, then the fix. PRs target `main`. `Closes #N` closes the issue
  when the PR merges. The Issues workflow reopens those issues when a merged
  `Reverts #N` PR takes the change out. Use `Refs #N` when a PR only touches
  one.
- Use `.github/pull_request_template.md` for every PR body, including PRs
  created through CLI tools; preserve its headings and remove instructional
  comments.
- Label every issue `area:desktop` or `area:conversation`, plus `windows`
  when Windows behaviour is involved. `ready` marks an issue the dispatcher
  can take; `ci-regression` marks the one the heavy tier files.
- When opening an issue, use the matching form in `.github/ISSUE_TEMPLATE/`.
  For CLI-created issues, use its field labels as Markdown headings. Blank
  issues remain available for cases neither form fits; issues are optional.

## Docs

- Write only what the code cannot answer.
- A DECISIONS.md bullet records a choice someone might revisit:
  `**Name.** Rule. Why: … Rejected: …`, four lines at most. Edit it in place,
  in the PR that changes the decision.
- This file changes in the same PR as the convention it describes.
