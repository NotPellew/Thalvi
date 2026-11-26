# Repository

Branches, CI, checks and releases.

- **Merge commits; only `main` is fixed.** PRs land as merge commits, and PR
  branches may be rewritten before merge. Why: the commits show the work being
  iterated, which is enough; no full audit trail. Rejected: squash merge (hides
  the iteration), a force-push block on all branches (more friction).
- **Work lands on `main`; a tag releases it.** PRs target `main`, the only long-lived branch, and only a tag on `main`
  publishes a release. An approved push to `main` publishes an `X.Y.Z-dev.N` prerelease, never Latest, for the Development
  update choice. Why: GitHub closes issues natively, and testers get fixes as they land. Rejected: a second
  integration branch (a workflow faked issue closing for it), a version commit per dev build.
- **One approving review.** The `main` ruleset requires one approving review. The `hold` workflow
  approves, as GitHub Actions, a PR the owner wrote or one that closes only fixes and changes no protected path; any other
  waits for a review, which on an agent's PR only the owner can give. Why: a check that failed until a label was added read
  as broken CI. Rejected: the `owner-ok` label, a pending status (agent runs wait on it), code owners (the owner's own PRs would need a bypass).
- **Issues are optional.** Why: in a 9-day solo build, PRs and the commit
  history are record enough. Rejected: an issue for every change.
- **`unsafe` only at native FFI boundaries.** The workspace denies `unsafe`; Windows host modules and
  Linux WebGPU/ONNX integration opt out narrowly, each block with `// SAFETY:`.
  Why: desktop control and GPU discovery/placement need raw native calls.
  Rejected: workspace `forbid` (prevents these integrations), `unsafe` allowed everywhere.
- **Commit checks run on `pull_request`.** They need only read access, and
  changes to them take effect in the PR that makes them. Rejected:
  `pull_request_target`, which runs the base branch's workflow definition.
- **Releases wait for approval.** `publish` runs in a `release` environment only the developer can approve. `verified` is only
  a pointer. A release checks the GitHub API for a successful heavy run on its commit and refuses otherwise; a development build
  (`X.Y.Z-dev.N`) needs only a commit on `main` and the approval. Why: agent tokens can push tags, and a tag builds and publishes
  to users. Rejected: a tag ruleset (the token acts as the developer, so any bypass for the developer also lets agents through).
- **The approved `publish` job signs for the updater.** It signs the downloaded packages with `tauri signer sign`
  after an `--ignore-scripts` install, checks each signature against the committed public key, and writes `latest.json`.
  Why: the key in the `release` environment never reaches a job that runs build scripts. Rejected: signing inside
  `tauri build` (`createUpdaterArtifacts`), which needs the key in both package jobs and fails PR builds without it.
- **Dependency and secret gates.** cargo-deny checks advisories, licences and sources; gitleaks scans
  history in CI and staged changes in a hook. Why: nothing else watched Rust advisories, the binaries link
  every crate, and on a public repo a pushed secret has leaked. Rejected: bans (nothing to ban), flagging
  unmaintained transitive crates (no upgrade exists), a hook that refuses when gitleaks is missing.
- **Rust caches are saved only on `main`.** The heavy tier runs on pushes to `main` and saves the Rust caches there; PRs,
  and mutation runs on the Linux one, only read them. Why: a PR can read only its own and its base branch's caches,
  so per-PR copies left every new PR cold and filled the 10 GB limit. Rejected: saving on every PR.
- **PRs run what changed; `main` runs everything.** A PR runs the steps its paths affect, and anything unmapped
  runs all, except Markdown outside `crates/` and `speech/`, which runs no unit; `heavy.yml` runs every gate plus mutants on `main`, nightly and on request. Why: PRs get a fast answer,
  and a slip past detection is caught on `main` with the commits since `verified`. Rejected: mutants on every PR.
- **Local checks run in three stages.** Editing runs the closest check; before pushing, each component `changed origin/main HEAD` names runs `gates` once, with `TESTS` and `BUNDLE=false`, and only `all` (an unmapped diff) builds a package locally; PR CI and `main` are unchanged (13 Nov 2026).
  Why: in 54 PR server jobs the AppImage took a median 135 s of a 261 s gate (53%), and a branch ran the full gate 2.3 times on average.
  Rejected: nextest, sccache or a shared target (no measurement points at them), a second path mapping, dropping the package from CI.
- **Windows packaging stays inside the Windows check.** NSIS packaging stays a step of `desktop-windows`, and installed-app smoke stays after that job (15 Nov 2026).
  Why: on 23 full cached runs the package step was a median 134 s (82–158), which clears #395's 2 minutes of wall time, but a second Windows job adds the setup the first job no longer pays, a median 92 s (73–106), over the 1 runner-minute cap on every run.
  Rejected: a parallel package job. Moving the step off the checks job does not make that step free; the net added runner time is the duplicated setup.
- **Mutation testing gates six high-risk desktop modules.** Heavy CI checks changes since `verified` in the files
  under `examine_globs` and fails on survivors (20 Nov 2026, #601).
  Why: a curated scope keeps the gate focused on desktop state, capture, delivery and settings.
  Rejected: advisory workspace-wide sweeps, Windows runs, local runs by default.
- **Property tests only where event order is the rule.** The hold hook and runner controls across captures,
  and the Settings window (`crates/thalvi-app/ui/explore`, fast-check over clicks, keys and waits).
  Why: random controls explore capture, cancellation, settings and shutdown orders beyond the bounded models,
  and click, key and status orders in Settings beyond handwritten tests. Rejected: properties everywhere.
- **Settings exploration uses fast-check and Playwright.** `explore/settings.ts` drives Settings in headless Chromium against `?fake`,
  under a paused page clock, with a fixed seed in `gates desktop-ui` and a random one in the heavy run (18 Nov 2026).
  Why: the prototype found #454, #531 and #533–#536 on its own at about 22 actions a second, and every shrunk trace replayed 3 of 3.
  Rejected: Bombadil 0.7.10, which takes no seed and whose shrinker replayed #454 in 0 of 3 tries.
- **Repeatable property cases, separate exploration.** Cargo defaults to 256 ChaCha cases with a fixed seed;
  exploratory runs override and record the seed, and failures become minimized regressions.
  Why: required checks need replayable inputs while exploration needs new ones. Rejected: fixed seeds alone.
- **Selective lint enforcement.** Vardli and Thalvi share one Rust lint set: pedantic except `missing_errors_doc` and
  `must_use_candidate`; production unwrap, expect, panic, todo and unimplemented denied; narrow reasoned expectations (16 Nov 2026).
  Why: catch mistakes without forcing artificial error paths, fragmented functions or docs that restate a signature.
  Rejected: all nursery lints, a blanket indexing ban (Vardli has one; #508 decides), lower hard complexity caps, suppression to pass a gate.
