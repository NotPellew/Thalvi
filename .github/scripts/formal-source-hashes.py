#!/usr/bin/env python3
"""Check source hashes for every source-mapped formal-model skill."""
import argparse
import hashlib
import json
from pathlib import Path
import sys


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    root = parser.parse_args().root.resolve()
    skills = sorted((root / '.agents/skills').glob('formal-*/source.json'))
    if not skills:
        print(f'formal source hashes: no formal-*/source.json files under {root}', file=sys.stderr)
        return 1

    drifted = False
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
                    drifted = True
        except (OSError, KeyError, TypeError, ValueError, json.JSONDecodeError) as error:
            print(f'INVALID {source.relative_to(root)}: {error}', file=sys.stderr)
            drifted = True

    if drifted:
        return 1
    print(f'PASS: source hashes match for {len(skills)} formal skills.')
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
