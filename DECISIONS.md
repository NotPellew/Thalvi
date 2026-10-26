# DECISIONS.md

Decisions in force, grouped by area. Each area stands alone; read only the
areas your change touches. Editing rules are in
[AGENTS.md](AGENTS.md#documentation-rules).

## Architecture

- **Rust server, TypeScript web client.** The Rust server owns sessions, model
  calls, and tools; the TypeScript client owns microphone, camera, playback,
  and UI. Why: browser media APIs are first-class in TypeScript and awkward
  through `web-sys`. Rejected: full Rust with a WASM UI (Leptos, Dioxus).
- **Phone browser first.** The client is a mobile web app served over HTTPS,
  which mobile browsers require for microphone and camera access. Why: no
  app-store builds or signing in a 9-day hackathon. Rejected for now: a
  native app; revisit once the web client works.

## Repository

- **Merge commits, history kept as written.** PRs land as merge commits and
  pushed commits are never rewritten. Why: judges and other participants see
  how the work progressed, with original SHAs and dates. Rejected: squash
  merge (one commit per PR), rebase merge (rewrites SHAs and commit dates).
- **No review gate.** The `main` ruleset requires PRs and passing checks but
  no approvals. Why: Thalvi has one author, and GitHub does not let authors
  approve their own PRs. Revisit if a second contributor joins.
- **Issues are optional.** Why: in a 9-day solo build, PRs and the commit
  history are record enough. Rejected: an issue for every change.
- **Commit checks run on `pull_request`.** They need only read access, and
  changes to them take effect in the PR that makes them. Rejected:
  `pull_request_target`, which runs the base branch's workflow definition.
