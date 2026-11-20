#!/usr/bin/env python3
"""Recorded formal sources must match HEAD and the sync commit, or the check fails."""
import hashlib
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / 'formal-source-hashes.py'
REDIRECTS = (
    'GIT_DIR',
    'GIT_WORK_TREE',
    'GIT_INDEX_FILE',
    'GIT_OBJECT_DIRECTORY',
    'GIT_ALTERNATE_OBJECT_DIRECTORIES',
)


def git(repo, *args):
    subprocess.run(
        ['git', '-C', str(repo), '-c', 'user.email=test@example.com', '-c', 'user.name=test', *args],
        check=True, capture_output=True, text=True,
    )


def rev(repo, spec='HEAD'):
    return subprocess.check_output(['git', '-C', str(repo), 'rev-parse', spec], text=True).strip()


class Provenance(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.repo = Path(self.temp.name)
        git(self.repo, 'init', '-q', '-b', 'main')

    def tearDown(self):
        self.temp.cleanup()

    def commit(self, message):
        git(self.repo, 'add', '-A')
        git(self.repo, 'commit', '-q', '-m', message)

    def source(self, sync, files):
        skill = self.repo / '.agents' / 'skills' / 'formal-demo'
        skill.mkdir(parents=True, exist_ok=True)
        inventory = {
            'sync_commit': sync,
            'files': {path: {'sha256': hashlib.sha256(data).hexdigest()} for path, data in files.items()},
        }
        (skill / 'source.json').write_text(json.dumps(inventory))

    def check(self, env=None):
        return subprocess.run(
            ['python3', str(SCRIPT), '--root', str(self.repo)],
            capture_output=True, text=True, env=env,
        )

    def assert_fails(self, result, phrase):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(phrase, result.stderr)
        self.assertNotIn('git provenance match', result.stdout)

    def test_valid_provenance_passes(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        self.source(rev(self.repo), {'crates/a.rs': b'code\n'})
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('git provenance match for 1 formal skills', result.stdout)

    def test_current_checkout_provenance_passes(self):
        result = subprocess.run(['python3', str(SCRIPT)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('git provenance match for 3 formal skills', result.stdout)

    def test_malformed_sync_commit_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        for value in ('nope', 'abc', 'g' * 40, 'a' * 39, 'a' * 41):
            with self.subTest(value=value):
                self.source(value, {'crates/a.rs': b'code\n'})
                self.assert_fails(self.check(), 'malformed sync_commit')

    def test_all_zero_sync_commit_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        self.source('0' * 40, {'crates/a.rs': b'code\n'})
        self.assert_fails(self.check(), 'all-zero sync_commit')

    def test_missing_sync_commit_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        skill = self.repo / '.agents' / 'skills' / 'formal-demo'
        skill.mkdir(parents=True)
        recorded = hashlib.sha256(b'code\n').hexdigest()
        for body in (
            {'files': {'crates/a.rs': {'sha256': recorded}}},
            {'sync_commit': None, 'files': {'crates/a.rs': {'sha256': recorded}}},
            {'sync_commit': '', 'files': {'crates/a.rs': {'sha256': recorded}}},
        ):
            with self.subTest(body=body):
                (skill / 'source.json').write_text(json.dumps(body))
                self.assert_fails(self.check(), 'missing sync_commit')

    def test_non_commit_sync_commit_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        blob = subprocess.check_output(['git', '-C', str(self.repo), 'hash-object', str(path)], text=True).strip()
        self.source(blob, {'crates/a.rs': b'code\n'})
        self.assert_fails(self.check(), 'not a commit')

    def test_non_ancestor_sync_commit_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        git(self.repo, 'checkout', '-q', '--orphan', 'side')
        self.commit('side')
        side = rev(self.repo)
        git(self.repo, 'checkout', '-q', 'main')
        self.source(side, {'crates/a.rs': b'code\n'})
        self.assert_fails(self.check(), 'not an ancestor of HEAD')

    def test_sync_commit_blob_mismatch_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'old\n')
        self.commit('old')
        sync = rev(self.repo)
        path.write_bytes(b'new\n')
        self.commit('new')
        self.source(sync, {'crates/a.rs': b'new\n'})
        self.assert_fails(self.check(), 'blob at sync_commit')

    def test_file_missing_at_sync_commit_fails(self):
        (self.repo / 'README').write_text('notes\n')
        self.commit('base')
        sync = rev(self.repo)
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.source(sync, {'crates/a.rs': b'code\n'})
        self.assert_fails(self.check(), 'missing at sync_commit')

    def test_working_tree_drift_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        self.source(rev(self.repo), {'crates/a.rs': b'code\n'})
        path.write_bytes(b'edited\n')
        self.assert_fails(self.check(), 'SOURCE DRIFT')

    def test_missing_working_tree_file_fails(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        self.source(rev(self.repo), {'crates/a.rs': b'code\n'})
        path.unlink()
        result = self.check()
        self.assert_fails(result, 'SOURCE DRIFT')
        self.assertIn('found missing', result.stderr)

    def test_git_redirects_do_not_hide_the_repository(self):
        path = self.repo / 'crates' / 'a.rs'
        path.parent.mkdir(parents=True)
        path.write_bytes(b'code\n')
        self.commit('base')
        self.source(rev(self.repo), {'crates/a.rs': b'code\n'})
        other = Path(self.temp.name) / 'other'
        other.mkdir()
        git(other, 'init', '-q')
        env = os.environ.copy()
        env['GIT_DIR'] = str(other / '.git')
        env['GIT_WORK_TREE'] = str(other)
        env['GIT_INDEX_FILE'] = str(other / 'index')
        env['GIT_OBJECT_DIRECTORY'] = str(other / 'objects')
        env['GIT_ALTERNATE_OBJECT_DIRECTORIES'] = str(other / 'alt')
        result = self.check(env)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('git provenance match for 1 formal skills', result.stdout)


if __name__ == '__main__':
    unittest.main()
