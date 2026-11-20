#!/usr/bin/env python3
"""Run the formal checkers a pull request or a heavy run selects.

EVENT=pull_request reads BASE and HEAD; an unusable comparison runs every
skill. push, schedule, workflow_dispatch, and workflow_call run every skill
when SCOPE is all (or unset) and none otherwise. Any other event, including
an unset one, runs every skill.
"""
import argparse
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

# A caller can point Git at another checkout. Selection has to read this one.
REDIRECTS = (
    'GIT_DIR',
    'GIT_WORK_TREE',
    'GIT_INDEX_FILE',
    'GIT_OBJECT_DIRECTORY',
    'GIT_ALTERNATE_OBJECT_DIRECTORIES',
)
HEAVY = {'push', 'schedule', 'workflow_dispatch', 'workflow_call'}


def git_env():
    env = os.environ.copy()
    for name in REDIRECTS:
        env.pop(name, None)
    return env


def git(root, *args):
    return subprocess.run(
        ['git', '-C', str(root), *args],
        capture_output=True,
        env=git_env(),
    )


def discover(root):
    base = root / '.agents' / 'skills'
    if not base.is_dir():
        return []
    skills = []
    for skill in sorted(base.glob('formal-*')):
        if skill.is_dir() and skill.name != 'formal-setup' and (skill / 'scripts' / 'check.py').is_file():
            skills.append(skill)
    return skills


def covers(skill):
    globs = []
    for line in (skill / 'SKILL.md').read_text(errors='replace').splitlines():
        if not line.startswith('Covers:'):
            continue
        for token in line[len('Covers:'):].split():
            if len(token) >= 2 and token[0] == token[-1] and token[0] in "'\"":
                token = token[1:-1]
            if token:
                globs.append(token)
    return globs


def _atom_quantifier(parts, quantifier):
    if not parts:
        raise ValueError('quantifier without atom')
    parts[-1] = '(?:' + parts[-1] + ')' + quantifier


def compile_glob(pattern):
    """GitHub path filter: `?` and `+` quantify the preceding atom, `*` skips `/`, `**` does not."""
    parts = []
    index, size = 0, len(pattern)
    while index < size:
        if pattern.startswith('/**/', index):
            # `/**/` matches `/` itself, so `docs/**/*.md` matches `docs/README.md`.
            parts.append('(?:/.*)?/')
            index += 4
            continue
        if index == 0 and pattern.startswith('**/'):
            parts.append('(?:.*/)?')
            index += 3
            continue
        if pattern.startswith('**', index):
            parts.append('.*')
            index += 2
            continue
        char = pattern[index]
        if char == '*':
            parts.append('[^/]*')
            index += 1
            continue
        if char == '[':
            end = pattern.find(']', index + 1)
            if end == -1:
                raise ValueError('unclosed class')
            body = pattern[index + 1:end]
            if not body or not re.fullmatch(r'[A-Za-z0-9-]+', body):
                raise ValueError('bad class')
            parts.append('[' + body + ']')
            index = end + 1
            continue
        if char == '\\':
            if index + 1 >= size:
                raise ValueError('trailing escape')
            parts.append(re.escape(pattern[index + 1]))
            index += 2
            continue
        if char in '?+':
            _atom_quantifier(parts, char)
            index += 1
            continue
        parts.append(re.escape(char))
        index += 1
    try:
        return re.compile(''.join(parts))
    except re.error as error:
        raise ValueError(str(error)) from error


def path_included(path, patterns):
    """Last matching pattern wins. A pattern is included unless a later `!` pattern matches it."""
    included = False
    for raw in patterns:
        negate = raw.startswith('!')
        body = raw[1:] if negate else raw
        if body == '':
            raise ValueError('empty pattern')
        if compile_glob(body).fullmatch(path) is not None:
            included = not negate
    return included


def matches(skill, paths):
    globs = covers(skill)
    if not globs:
        return False
    try:
        return any(path_included(path, globs) for path in paths)
    except ValueError:
        # An unreadable Covers line would otherwise drop the skill on a real change.
        return True


def changed_files(root, base, head):
    if not base or not head:
        return None
    result = git(root, 'diff', '--name-only', '--no-renames', '-z', f'{base}...{head}', '--')
    if result.returncode != 0:
        return None
    return [name.decode() for name in result.stdout.split(b'\0') if name]


def choose(root, event, scope, base, head):
    skills = discover(root)
    if event == 'pull_request':
        files = changed_files(root, base, head)
        if files is None:
            return skills, 'unusable'
        return [skill for skill in skills if matches(skill, files)], 'diff'
    if event in HEAVY and scope not in ('', 'all'):
        return [], 'scoped'
    # Local, heavy at scope all, and any event this script does not know: run every skill.
    return skills, 'all'


def run(root, argv):
    print('==> ' + ' '.join(argv), flush=True)
    return subprocess.run(argv, cwd=root).returncode


def chart_fresh(root, skill):
    chart = skill / 'chart.mmd'
    with tempfile.TemporaryDirectory(prefix='thalvi-formal-chart-') as scratch:
        generated = Path(scratch) / chart.name
        code = run(root, [
            sys.executable, str(skill / 'scripts' / 'check.py'), 'chart', '--out', str(generated),
        ])
        if code != 0:
            print(f'CHART {skill.name}: generator exited {code}', file=sys.stderr)
            return code
        if not (chart.is_file() and generated.is_file()) or generated.read_bytes() != chart.read_bytes():
            print(f'STALE CHART {skill.name}: chart.mmd does not match the model', file=sys.stderr)
            return 1
    print(f'CHART {skill.name}: matches the model')
    return 0


def execute(root, skill):
    check = skill / 'scripts' / 'check.py'
    code = run(root, [sys.executable, str(check)])
    if code != 0:
        return code
    code = run(root, [sys.executable, str(check), '--self-test'])
    if code != 0:
        return code
    if any(skill.glob('*.tla')):
        code = chart_fresh(root, skill)
        if code != 0:
            return code
    helper = skill / 'scripts' / 'test_check.py'
    if helper.is_file():
        return run(root, [sys.executable, str(helper)])
    return 0


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--root', type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument('--plan', action='store_true', help='print skills=<names> and run nothing')
    args = parser.parse_args()
    root = args.root.resolve()
    chosen, reason = choose(
        root,
        os.environ.get('EVENT', ''),
        os.environ.get('SCOPE', ''),
        os.environ.get('BASE', ''),
        os.environ.get('HEAD', ''),
    )
    if reason == 'unusable':
        print('formal checks: PR comparison is unusable, so every formal skill runs', file=sys.stderr)
    if args.plan:
        print('skills=' + ','.join(skill.name for skill in chosen))
        return 0
    if not chosen:
        print('formal checks: no covered change; checkers not run')
        return 0
    print('formal checks: running ' + ', '.join(skill.name for skill in chosen))
    for skill in chosen:
        code = execute(root, skill)
        if code != 0:
            return code
    return 0


if __name__ == '__main__':
    raise SystemExit(main())
