#!/usr/bin/env python3
"""Run the hold keys' bounded exhaustive check against a snapshot of the real code."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile

SKILL = Path(__file__).resolve().parents[1]
ROOT = SKILL.parents[2]
CACHE = (Path(os.environ.get('XDG_CACHE_HOME', Path.home() / '.cache')) / 'thalvi-formal').resolve()
HOLD = 'crates/thalvi-desktop/src/hold.rs'
PROPERTIES = ('EachPressCountedOnce', 'HoldFollowsItsKey', 'FreeKeyStartsCapture', 'ChordNeverHolds')
# Each mutation puts back one way `hold.rs` broke or could break, and the property it must break.
MUTATIONS = {
    'NoOvertakeCredit': ('            } else if phase == KeyPhase::Down {\n                KeyPhase::Up\n            } else {\n                KeyPhase::Overtaken(Instant::now())\n            },\n        );\n',
                        '            } else {\n                KeyPhase::Up\n            },\n        );\n', 'HoldFollowsItsKey'),
    'NoOtherKeyGuard': ('        // One Hold at a time: another Hold key means nothing until its own key is up.\n        if state.hold.is_some() && state.hold_row != Some(row) {\n            return false;\n        }\n',
                        '', 'HoldFollowsItsKey'),
    'UncountedLoss': ('                self.lost += 1;\n                continue;\n',
                        '                continue;\n', 'EachPressCountedOnce'),
    'ForgetIgnoredKey': ("        // Every key follows its own events, even while another key's Hold ignores them.\n        state.phases.insert(\n            row,\n            if down {\n                if overtaken {\n                    KeyPhase::Up\n                } else {\n                    KeyPhase::Down\n                }\n            } else if phase == KeyPhase::Down {\n                KeyPhase::Up\n            } else {\n                KeyPhase::Overtaken(Instant::now())\n            },\n        );\n        if mode != KeyMode::Hold {\n            if !down {\n                return false;\n            }\n            state.presses += 1;\n            state.kind = action;\n            state.mode = mode;\n            state.since = since;\n            state.push(Impulse {\n                number: state.presses,\n                row,\n                action,\n                mode,\n                at: Order::next(),\n                release: Cutoff::default(),\n                since,\n            });\n            return true;\n        }\n        // One Hold at a time: another Hold key means nothing until its own key is up.\n        if state.hold.is_some() && state.hold_row != Some(row) {\n            return false;\n        }\n",
                        "        // One Hold at a time: another Hold key means nothing until its own key is up.\n        if state.hold.is_some() && state.hold_row != Some(row) {\n            return false;\n        }\n        // Every key follows its own events, even while another key's Hold ignores them.\n        state.phases.insert(\n            row,\n            if down {\n                if overtaken {\n                    KeyPhase::Up\n                } else {\n                    KeyPhase::Down\n                }\n            } else if phase == KeyPhase::Down {\n                KeyPhase::Up\n            } else {\n                KeyPhase::Overtaken(Instant::now())\n            },\n        );\n        if mode != KeyMode::Hold {\n            if !down {\n                return false;\n            }\n            state.presses += 1;\n            state.kind = action;\n            state.mode = mode;\n            state.since = since;\n            state.push(Impulse {\n                number: state.presses,\n                row,\n                action,\n                mode,\n                at: Order::next(),\n                release: Cutoff::default(),\n                since,\n            });\n            return true;\n        }\n", 'FreeKeyStartsCapture'),
    'ChordIgnored': ('                state.altgr.insert(row, AltGrWait::Chord);\n',
                        '', 'ChordNeverHolds'),
    'StaleWaitConfirms': ('            if current != wait {\n                return false;\n            }\n',
                        '', 'ChordNeverHolds'),
    'ReleaseCreditsWaiting': ('                AltGrWait::Idle | AltGrWait::Holding => {\n                    Self::step(state, row, bind.action, bind.mode, false, None)\n                }\n                AltGrWait::Waiting { .. } | AltGrWait::Chord => false,\n',
                        '                AltGrWait::Idle\n                | AltGrWait::Holding\n                | AltGrWait::Waiting { .. }\n                | AltGrWait::Chord => {\n                    Self::step(state, row, bind.action, bind.mode, false, None)\n                }\n', 'FreeKeyStartsCapture'),
}


def source_check():
    if CACHE.is_relative_to(ROOT):
        raise RuntimeError('XDG_CACHE_HOME must point outside the repository')
    inventory = json.loads((SKILL / 'source.json').read_text())
    changed = [p for p, item in inventory['files'].items()
               if not (ROOT / p).is_file()
               or hashlib.sha256((ROOT / p).read_bytes()).hexdigest() != item['sha256']]
    if changed:
        raise RuntimeError('SOURCE DRIFT: read the skill, update the harness from these files, '
                           'then update source.json: ' + ', '.join(changed))
    print('Source matches recorded hashes; sync commit ' + inventory['sync_commit'], flush=True)


def snapshot(evidence, label, mutation=None):
    """Copies the tracked crates and the harness into a new directory, mutated if asked."""
    root = evidence / label
    root.mkdir()
    for name in ('Cargo.toml', 'Cargo.lock', 'rust-toolchain.toml'):
        shutil.copyfile(ROOT / name, root / name)
    tracked = subprocess.check_output(['git', 'ls-files', '-z', 'crates'], cwd=ROOT).decode().split('\0')
    for relative in filter(None, tracked):
        target = root / relative
        target.parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / relative, target)
    if mutation:
        old, new, _ = MUTATIONS[mutation]
        source = (root / HOLD).read_text()
        if source.count(old) != 1:
            raise RuntimeError(f'mutation {mutation} no longer matches {HOLD}; update it from the code')
        (root / HOLD).write_text(source.replace(old, new))
    shutil.copyfile(SKILL / 'harness.rs', root / 'crates/thalvi-desktop/tests/formal_hold_keys.rs')
    return root


def run(evidence, label, mutation=None):
    """Runs the harness once; returns the broken properties."""
    root = snapshot(evidence, label, mutation)
    environment = os.environ.copy()
    environment['TMPDIR'] = str(evidence)
    environment.setdefault('CARGO_TARGET_DIR', str(CACHE / 'hold-keys-target'))
    command = ['cargo', 'test', '--offline', '--locked', '-p', 'thalvi-desktop',
               '--test', 'formal_hold_keys', '--', '--nocapture']
    with (root / 'harness.txt').open('w') as log:
        done = subprocess.run(command, cwd=root, env=environment, stdout=log, stderr=subprocess.STDOUT,
                              timeout=600, check=False)
    output = (root / 'harness.txt').read_text()
    broken = re.findall(r'^COUNTEREXAMPLE (\w+)$', output, re.M)
    holds = re.findall(r'^HOLDS (\w+)$', output, re.M)
    if sorted(broken + holds) != sorted(PROPERTIES):
        raise RuntimeError(f'the harness did not report every property; read {root / "harness.txt"}')
    if done.returncode != (101 if broken else 0):
        raise RuntimeError(f'harness exit {done.returncode} contradicts its properties; read {root / "harness.txt"}')
    count = re.search(r'^SCENARIOS (\d+)$', output, re.M)
    if count is None or int(count.group(1)) == 0:
        raise RuntimeError(f'the harness did not run any scenarios; read {root / "harness.txt"}')
    scenarios = count.group(1)
    print(f'{label}: {scenarios} scenarios; '
          + ('COUNTEREXAMPLE ' + ' '.join(broken) if broken else 'PASS')
          + f' ({root / "harness.txt"})', flush=True)
    for block in re.findall(r'^COUNTEREXAMPLE \w+\n(?:.+\n)+?=> .+$', output, re.M):
        print(block, flush=True)
    return broken


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--self-test', action='store_true',
                        help='check that each mutation of hold.rs breaks its property')
    args = parser.parse_args()
    source_check()
    evidence = Path(tempfile.mkdtemp(prefix='thalvi-formal-hold-', dir='/var/tmp'))
    print(f'EVIDENCE={evidence}', flush=True)
    if args.self_test:
        for mutation, (_, _, property_name) in MUTATIONS.items():
            if property_name not in run(evidence, mutation, mutation):
                raise RuntimeError(f'{mutation} should break {property_name}')
        print('SELF-TEST PASS: every mutation broke its property. '
              'Run without --self-test for the verdict on the real code.')
        return 0
    broken = run(evidence, 'faithful')
    print(f'FAIL: {", ".join(broken)} broken on the real code; replay with SKILL.md.' if broken
          else 'PASS: every property holds for every sequence within the bounds; see SKILL.md.')
    return 1 if broken else 0


if __name__ == '__main__':
    try:
        sys.exit(main())
    except (RuntimeError, OSError, subprocess.TimeoutExpired) as error:
        print(f'BLOCKED: {error}', file=sys.stderr)
        sys.exit(2)
