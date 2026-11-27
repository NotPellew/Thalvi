#!/usr/bin/env python3
"""A formal run must follow the diff, and a stale chart must fail closed."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / 'formal-checks.py'
SKILLS = {
    'formal-capture-cancel': 'crates/thalvi-app/src/runner.rs',
    'formal-config-save': 'crates/thalvi-app/src/settings.rs',
    'formal-hold-keys': 'crates/thalvi-desktop/src/hold.rs',
}
CHARTS = {'formal-capture-cancel', 'formal-config-save'}
ALL = 'formal-capture-cancel,formal-config-save,formal-hold-keys'
CHECK_PY = """import os
import sys
from pathlib import Path

skill = Path(__file__).resolve().parents[1]
if len(sys.argv) > 1 and sys.argv[1] == 'chart':
    mode = 'CHART'
elif '--self-test' in sys.argv:
    mode = 'SELF_TEST'
else:
    mode = 'CHECK'
ran = skill / 'ran.txt'
previous = ran.read_text() if ran.is_file() else ''
ran.write_text(previous + mode + '\\n')
if mode == 'CHART':
    output = Path(sys.argv[sys.argv.index('--out') + 1])
    generated = b'graph\\n'
    if os.environ.get('FORMAL_FAKE_STALE') == '1':
        generated += b'drift\\n'
    output.write_bytes(generated)
raise SystemExit(int(os.environ.get('FORMAL_FAKE_EXIT_' + mode, '0')))
"""
HELPER_PY = """import os
import sys
from pathlib import Path

skill = Path(__file__).resolve().parents[1]
ran = skill / 'ran.txt'
previous = ran.read_text() if ran.is_file() else ''
ran.write_text(previous + 'HELPER\\n')
raise SystemExit(int(os.environ.get('FORMAL_FAKE_EXIT_HELPER', '0')))
"""


def git(repo, *args):
    subprocess.run(
        ['git', '-C', str(repo), '-c', 'user.email=test@example.com', '-c', 'user.name=test', *args],
        check=True, capture_output=True, text=True,
    )


def rev(repo):
    return subprocess.check_output(['git', '-C', str(repo), 'rev-parse', 'HEAD'], text=True).strip()


class Selection(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name)
        git(self.repo, 'init', '-q', '-b', 'main')
        for name, covered in SKILLS.items():
            skill = self.repo / '.agents' / 'skills' / name
            (skill / 'scripts').mkdir(parents=True)
            (skill / 'scripts' / 'check.py').write_text(CHECK_PY)
            (skill / 'SKILL.md').write_text(
                f"Covers: {covered} '.agents/skills/{name}/**'\n"
            )
            target = self.repo / covered
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text('original\n')
            if name in CHARTS:
                (skill / 'Model.tla').touch()
                (skill / 'chart.mmd').write_text('graph\n')
            if name == 'formal-hold-keys':
                (skill / 'scripts' / 'test_check.py').write_text(HELPER_PY)
        (self.repo / 'README.md').write_text('readme\n')
        git(self.repo, 'add', '-A')
        git(self.repo, 'commit', '-q', '-m', 'base')
        self.base = rev(self.repo)

    def tearDown(self):
        self.temp.cleanup()

    def change(self, relative):
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(path.read_text() + 'changed\n' if path.is_file() else 'changed\n')
        git(self.repo, 'add', '-A')
        git(self.repo, 'commit', '-q', '-m', 'change')
        return rev(self.repo)

    def invoke(self, event, scope='', base=None, head=None, extra=None, plan=False):
        env = os.environ.copy()
        for key in (
            'EVENT', 'SCOPE', 'BASE', 'HEAD', 'GIT_DIR', 'GIT_WORK_TREE', 'GIT_INDEX_FILE',
            'GIT_OBJECT_DIRECTORY', 'GIT_ALTERNATE_OBJECT_DIRECTORIES',
            'FORMAL_FAKE_STALE', 'FORMAL_FAKE_EXIT_CHECK', 'FORMAL_FAKE_EXIT_SELF_TEST',
            'FORMAL_FAKE_EXIT_CHART', 'FORMAL_FAKE_EXIT_HELPER',
        ):
            env.pop(key, None)
        env.update(EVENT=event, SCOPE=scope, BASE='' if base is None else base, HEAD='' if head is None else head)
        if extra:
            env.update(extra)
        command = ['python3', str(SCRIPT), '--root', str(self.repo)]
        if plan:
            command.append('--plan')
        return subprocess.run(command, capture_output=True, text=True, env=env)

    def ran(self, name):
        path = self.repo / '.agents' / 'skills' / name / 'ran.txt'
        return path.read_text() if path.is_file() else ''

    def test_uncovered_pull_request_skips_checkers(self):
        for relative in ('README.md', 'crates/thalvi-desktop/src/hold.rs.extra'):
            with self.subTest(relative=relative):
                head = self.change(relative)
                result = self.invoke('pull_request', scope='all', base=self.base, head=head)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('checkers not run', result.stdout)
                for name in SKILLS:
                    self.assertEqual(self.ran(name), '')
                self.base = head

    def test_pull_request_excludes_changes_only_on_the_base_branch(self):
        git(self.repo, 'switch', '-q', '-c', 'topic')
        head = self.change('README.md')
        git(self.repo, 'switch', '-q', 'main')
        base = self.change('crates/thalvi-desktop/src/hold.rs')
        git(self.repo, 'switch', '-q', 'topic')
        result = self.invoke('pull_request', scope='all', base=base, head=head, plan=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, 'skills=\n')

    def test_covered_pull_request_runs_only_the_matching_checker(self):
        cases = (
            ('crates/thalvi-app/src/runner.rs', 'formal-capture-cancel', 'CHECK\nSELF_TEST\nCHART\n'),
            ('crates/thalvi-app/src/settings.rs', 'formal-config-save', 'CHECK\nSELF_TEST\nCHART\n'),
            ('crates/thalvi-desktop/src/hold.rs', 'formal-hold-keys', 'CHECK\nSELF_TEST\nHELPER\n'),
            ('.agents/skills/formal-hold-keys/SKILL.md', 'formal-hold-keys', 'CHECK\nSELF_TEST\nHELPER\n'),
        )
        for relative, chosen, expected in cases:
            with self.subTest(relative=relative):
                for name in SKILLS:
                    ran = self.repo / '.agents' / 'skills' / name / 'ran.txt'
                    if ran.exists():
                        ran.unlink()
                head = self.change(relative)
                result = self.invoke('pull_request', scope='all', base=self.base, head=head)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(self.ran(chosen), expected)
                for name in SKILLS:
                    if name != chosen:
                        self.assertEqual(self.ran(name), '')
                self.base = head

    def test_unusable_pull_request_runs_every_checker(self):
        cases = (
            ('', self.base),
            (self.base, ''),
            (self.base, 'deadbeef'),
            ('not-a-commit', self.base),
        )
        for base, head in cases:
            with self.subTest(base=base, head=head):
                for name in SKILLS:
                    ran = self.repo / '.agents' / 'skills' / name / 'ran.txt'
                    if ran.exists():
                        ran.unlink()
                result = self.invoke('pull_request', scope='all', base=base, head=head)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('PR comparison is unusable', result.stderr)
                for name in SKILLS:
                    self.assertIn('CHECK\n', self.ran(name))

    def test_scope_all_runs_every_checker_for_heavy_events(self):
        for event in ('', 'push', 'schedule', 'workflow_dispatch', 'workflow_call'):
            with self.subTest(event=event or 'local'):
                for name in SKILLS:
                    ran = self.repo / '.agents' / 'skills' / name / 'ran.txt'
                    if ran.exists():
                        ran.unlink()
                result = self.invoke(event, scope='all')
                self.assertEqual(result.returncode, 0, result.stderr)
                for name in SKILLS:
                    self.assertIn('CHECK\n', self.ran(name))
                plan = self.invoke(event, scope='all', plan=True)
                self.assertEqual(plan.returncode, 0, plan.stderr)
                self.assertEqual(plan.stdout, f'skills={ALL}\n')

    def test_scoped_heavy_skips_checkers(self):
        for event, scope in (
            ('workflow_dispatch', 'thalvi-desktop'),
            ('push', 'desktop-ui'),
            ('schedule', 'thalvi-app'),
            ('workflow_call', 'thalvi-desktop'),
        ):
            with self.subTest(event=event, scope=scope):
                result = self.invoke(event, scope=scope)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertIn('checkers not run', result.stdout)
                for name in SKILLS:
                    self.assertEqual(self.ran(name), '')

    def test_a_failing_checker_self_test_or_helper_fails_the_run_and_stops_there(self):
        cases = (
            ('crates/thalvi-app/src/settings.rs', {'FORMAL_FAKE_EXIT_CHECK': '6', 'FORMAL_FAKE_EXIT_SELF_TEST': '5'}, 6, 'formal-config-save', 'CHECK\n'),
            ('crates/thalvi-app/src/runner.rs', {'FORMAL_FAKE_EXIT_SELF_TEST': '5'}, 5, 'formal-capture-cancel', 'CHECK\nSELF_TEST\n'),
            ('crates/thalvi-desktop/src/hold.rs', {'FORMAL_FAKE_EXIT_HELPER': '7'}, 7, 'formal-hold-keys', 'CHECK\nSELF_TEST\nHELPER\n'),
        )
        for relative, extra, code, chosen, expected in cases:
            with self.subTest(relative=relative):
                head = self.change(relative)
                result = self.invoke('pull_request', scope='all', base=self.base, head=head, extra=extra)
                self.assertEqual(result.returncode, code, result.stderr)
                self.assertEqual(self.ran(chosen), expected)
                self.base = head
                (self.repo / '.agents' / 'skills' / chosen / 'ran.txt').unlink()

    def test_a_stale_chart_fails_and_leaves_chart_mmd_unchanged_and_a_missing_one_fails(self):
        chart = self.repo / '.agents' / 'skills' / 'formal-config-save' / 'chart.mmd'
        original = chart.read_bytes()
        head = self.change('crates/thalvi-app/src/settings.rs')
        result = self.invoke(
            'pull_request', scope='all', base=self.base, head=head,
            extra={'FORMAL_FAKE_STALE': '1'},
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('STALE CHART formal-config-save', result.stderr)
        self.assertEqual(chart.read_bytes(), original)

        chart.unlink()
        again = self.change('crates/thalvi-app/src/settings.rs')
        result = self.invoke('pull_request', scope='all', base=head, head=again)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn('STALE CHART formal-config-save', result.stderr)
        self.assertFalse(chart.exists())

    def test_git_redirect_cannot_force_every_checker(self):
        head = self.change('README.md')
        other = Path(self.temp.name) / 'other'
        other.mkdir()
        git(other, 'init', '-q')
        result = self.invoke(
            'pull_request', scope='all', base=self.base, head=head,
            extra={
                'GIT_DIR': str(other / '.git'),
                'GIT_WORK_TREE': str(other),
                'GIT_INDEX_FILE': str(other / 'index'),
                'GIT_OBJECT_DIRECTORY': str(other / 'objects'),
                'GIT_ALTERNATE_OBJECT_DIRECTORIES': str(other / 'alt'),
            },
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn('unusable', result.stderr)
        for name in SKILLS:
            self.assertEqual(self.ran(name), '')


if __name__ == '__main__':
    unittest.main()
