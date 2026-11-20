#!/usr/bin/env python3
"""Check source hashes and git provenance for every source-mapped formal skill."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

# A caller can point Git at another checkout. Provenance has to read this one.
REDIRECTS = (
    'GIT_DIR',
    'GIT_WORK_TREE',
    'GIT_INDEX_FILE',
    'GIT_OBJECT_DIRECTORY',
    'GIT_ALTERNATE_OBJECT_DIRECTORIES',
)
SYNC_COMMIT = re.compile(r'[0-9a-fA-F]{40}')
ZERO = '0' * 40


def git_env():
    env = os.environ.copy()
    for name in REDIRECTS:
        env.pop(name, None)
    return env


def git(root, *args, binary=False):
    return subprocess.run(
        ['git', '-C', str(root), *args],
        capture_output=True,
        env=git_env(),
        text=not binary,
    )


def provenance_errors(root, skill, inventory):
    """Errors proving sync_commit is not an ancestor that holds the recorded blobs."""
    label = skill.relative_to(root).as_posix()
    sync = inventory.get('sync_commit')
    if not isinstance(sync, str) or sync == '':
        return [f'PROVENANCE {label}: missing sync_commit']
    if not SYNC_COMMIT.fullmatch(sync):
        return [f'PROVENANCE {label}: malformed sync_commit']
    if sync.lower() == ZERO:
        return [f'PROVENANCE {label}: all-zero sync_commit']
    sync = sync.lower()
    kind = git(root, 'cat-file', '-t', sync)
    if kind.returncode != 0 or kind.stdout.strip() != 'commit':
        return [f'PROVENANCE {label}: sync_commit is not a commit']
    if git(root, 'merge-base', '--is-ancestor', sync, 'HEAD').returncode != 0:
        return [f'PROVENANCE {label}: sync_commit is not an ancestor of HEAD']
    errors = []
    for relative, entry in inventory['files'].items():
        expected = entry['sha256']
        blob = git(root, 'cat-file', 'blob', f'{sync}:{relative}', binary=True)
        if blob.returncode != 0:
            errors.append(f'PROVENANCE {label}: {relative} is missing at sync_commit')
            continue
        actual = hashlib.sha256(blob.stdout).hexdigest()
        if actual != expected:
            errors.append(
                f'PROVENANCE {label}: {relative} blob at sync_commit is {actual}, recorded {expected}'
            )
    return errors


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    root = parser.parse_args().root.resolve()
    skills = sorted((root / '.agents/skills').glob('formal-*/source.json'))
    if not skills:
        print(f'formal source hashes: no formal-*/source.json files under {root}', file=sys.stderr)
        return 1

    failed = False
    for source in skills:
        skill = source.parent
        try:
            inventory = json.loads(source.read_text())
            files = inventory['files']
            if not isinstance(files, dict) or not files:
                raise ValueError('source.json files must be a non-empty object')
            for relative, entry in files.items():
                path = root / relative
                expected = entry['sha256']
                if not path.is_file():
                    actual = 'missing'
                else:
                    actual = hashlib.sha256(path.read_bytes()).hexdigest()
                if actual != expected:
                    print(
                        f'SOURCE DRIFT {skill.relative_to(root)}: {relative} '
                        f'(expected {expected}, found {actual}); follow '
                        f'{(skill / "SKILL.md").relative_to(root)} “When the covered code changes”.',
                        file=sys.stderr,
                    )
                    failed = True
            for error in provenance_errors(root, skill, inventory):
                print(error, file=sys.stderr)
                failed = True
        except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            print(f'INVALID {source.relative_to(root)}: {error}', file=sys.stderr)
            failed = True

    if failed:
        return 1
    print(f'PASS: source hashes and git provenance match for {len(skills)} formal skills.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
