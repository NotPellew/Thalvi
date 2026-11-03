#!/usr/bin/env python3
"""Run the real engine and source-extracted runner against an event-gated fake close."""
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

sys.dont_write_bytecode = True
from check import CACHE, ROOT, SKILL, source_check


def main():
    source_check()
    evidence = Path(tempfile.mkdtemp(prefix='thalvi-cancel-repro-', dir='/var/tmp'))
    snapshot = evidence / 'source'
    snapshot.mkdir()
    for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
        shutil.copyfile(ROOT / name, snapshot / name)
    tracked = subprocess.check_output(['git', 'ls-files', '-z', 'crates'], cwd=ROOT).decode().split('\0')
    for relative in filter(None, tracked):
        target = snapshot / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    runner = (ROOT / 'crates/thalvi-app/src/runner.rs').read_text()
    pieces = [
        runner[runner.index('#[derive(Debug)]\npub enum Control'):runner.index('/// The saved microphone choice')],
        runner[runner.index('#[derive(Default)]\nstruct Requests'):runner.index('/// Runs `capture`')],
        runner[runner.index('async fn drive<O>'):runner.index('/// Carries one engine')],
    ]
    template = (SKILL / 'scripts/repro.rs').read_text()
    test = template.replace('// INSERT_RUNNER_SOURCE', '\n'.join(pieces))
    (snapshot / 'crates/thalvi-desktop/tests/formal_cancel_repro.rs').write_text(test)
    environment = os.environ.copy()
    environment['TMPDIR'] = str(evidence)
    environment['CARGO_TARGET_DIR'] = str(CACHE / 'repro-target')
    print(f'EVIDENCE={evidence}', flush=True)
    command = ['cargo', 'test', '--offline', '--locked', '-p', 'thalvi-desktop',
               '--test', 'formal_cancel_repro', '--', '--nocapture']
    with (evidence / 'repro.txt').open('w') as log:
        done = subprocess.run(command, cwd=snapshot, env=environment,
                              stdout=log, stderr=subprocess.STDOUT, timeout=300, check=False)
    output = (evidence / 'repro.txt').read_text()
    if done.returncode == 0:
        print('PASS: the real-code replay did not deliver after Cancel')
        return 0
    if 'CANCEL_REACHED_DELIVERY' in output:
        print('FINDING: Cancel was consumed, then the real engine returned Accepted and the fake host delivered.')
        print(f'Read {evidence / "repro.txt"}')
        return 1
    print(f'BLOCKED: replay did not reach its assertion; read {evidence / "repro.txt"}')
    return 2


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, ValueError, subprocess.TimeoutExpired) as error:
        print(f'BLOCKED: {error}', file=sys.stderr)
        sys.exit(2)
