# AGENTS.md

Working rules for this repository, for humans and agents alike. Product
direction and architecture decisions live in [DECISIONS.md](DECISIONS.md); the
user-facing intro lives in [README.md](README.md). Conventions change in the
same PR that changes them.

## Toolchain

- Every toolchain is pinned in the repository: Rust via `rust-toolchain.toml`,
  Node via `.node-version`. CI reads the same pins.
- npm is the package manager; lockfiles are committed.
- No other global tooling is required for builds or CI.

## The gates

Run these before pushing a PR. CI runs exactly these; a command that was not
run has not passed.

No component is scaffolded yet, so there are no code gates. The PR that
scaffolds a component adds, together: its toolchain pin, its gate commands in
this section, its CI job, and that job's name to the required status checks of
the `main` ruleset on GitHub.

## Rust code style

- No inline comments, in tests too. `///` documents an item's contract and
  any non-obvious reason for it, whatever its visibility; `//!` gives
  module-level rationale; comments that restate the code are deleted.
- A reason that belongs to one line goes into a named function or constant
  whose `///` says it, or into the `reason` of an `#[expect]`. In tests, the
  test name says the scenario.
- Every module starts with `//!`, and its first line is a complete sentence
  saying what the module owns. `grep -rm1 '^//!' crates/*/src` is the module
  map; read it before opening large files.
- `src/lib.rs` is a facade: `mod` declarations plus selective `pub use`. Logic
  lives in submodules; internals default to `pub(crate)`.
- One module style per crate: `foo.rs` beside `foo/`; no `mod.rs`.
- `crates/` is flat; directory name = crate name.
- Shared dependency versions and lints are inherited from the workspace root;
  members use `.workspace = true`.
- Tests: unit tests in `#[cfg(test)] mod tests`, integration tests in
  `tests/`. Unit tests longer than about 150 lines move to `foo/tests.rs`.
- `unwrap`/`expect` warn outside tests. Cross-module boundaries return
  `Result`.
- The workspace `[workspace.lints]` table is the single source of lint policy.
  Never enable `nursery` or `restriction` wholesale; add individual lints
  deliberately, with a reason, at the workspace root only.

## Git workflow

- `main` changes only through pull requests. A GitHub ruleset enforces it:
  squash merge only, linear history, no force-push or deletion, required
  checks must pass. No approval is required; see DECISIONS.md.
- Never rewrite `main`'s history: it is the hackathon's provenance record.
- Issues are optional. When one exists, the branch references it and the PR
  body says `Closes #N` or `Refs #N`.
- Branch names: `<type>/<slug>`, e.g. `feat/voice-barge-in`, `fix/timer-drift`
  — three hyphenated words or fewer.
- Commits and PR titles follow Conventional Commits: `type(scope): summary`,
  the summary starting lowercase. Types: `feat fix docs refactor test chore ci
  build perf`. Scopes, when used, come from: `server web ci deps docs`. The
  lists live here and in [pr-title.yml](.github/workflows/pr-title.yml);
  change both together.
- The squash commit takes the PR title, so the title must follow the
  convention — CI checks it. The local commit-msg hook mirrors the rule; run
  `git config core.hooksPath .githooks` once per clone.

## Hackathon provenance

- All code is written during the hackathon (October 26 – November 4, 2026).
  Nothing is copied from other repositories, including the author's earlier
  prototype; third-party code enters only as declared dependencies.
- AI assistance is disclosed in README.md. Commits made with an agent carry
  its `Co-Authored-By` trailer.

## Documentation rules

- DECISIONS.md holds the decisions in force, grouped by area. It is edited in
  place, in the same PR that makes, changes, or drops a decision. Git history
  and the PR description record how it changed; the file does not.
- Every DECISIONS.md area must make sense without the others; read only the
  areas your change touches. Keep the file under 300 lines.
- A decision is a choice between real alternatives that someone might later
  revisit. Its bullet is `**Name.** Rule. Why: … Rejected: …`, at most four
  lines. Implementation detail goes in the PR, the commit, or `//!` docs.
- README.md: update when the shape of the project changes.
- Write only what the code cannot answer. No feature catalogues, no file
  listings, no PR summaries committed as docs.

## Verification discipline

- A test that needs a timeout to pass is wrong. A bug fix adds a case that
  fails without the fix.
- Do not open a PR with checks you have not run locally or examined in CI.
- Tests never call Meta's model APIs or any other paid service: model calls
  sit behind an interface that tests replace with fakes. CI holds no API
  secrets.
- `agent-browser` is installed and can be used to verify the web client. Run
  `agent-browser --help` for usage information.

## Never commit

- `.env` or any secret — if one is ever committed, rotate every credential in
  it; removing the file is not remediation. `.env.example` documents
  variables only.
- Model weights, checkpoints, datasets, audio or camera captures, logs, or
  other generated runtime data.
- Hostnames, local paths, or details of the machines the project runs on.
- PR evidence (screenshots, recordings) — upload it to the PR instead.
