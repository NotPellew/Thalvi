#!/usr/bin/env python3
"""A failed or empty harness must not become a formal pass."""
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

sys.dont_write_bytecode = True
import check


class HarnessVerdict(unittest.TestCase):
    def test_process_and_property_verdicts_must_agree(self):
        for exit_code, count, broken, blocked in (
            (0, 1, False, False),
            (101, 1, False, True),
            (101, 1, True, False),
            (-9, 1, True, True),
            (0, 0, False, True),
            (0, None, False, True),
        ):
            with self.subTest(exit=exit_code, count=count, broken=broken), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)

                def process(*args, **kwargs):
                    if count is not None:
                        kwargs['stdout'].write(f'SCENARIOS {count}\n')
                    for index, name in enumerate(check.PROPERTIES):
                        verdict = 'COUNTEREXAMPLE' if broken and index == 0 else 'HOLDS'
                        kwargs['stdout'].write(f'{verdict} {name}\n')
                    return SimpleNamespace(returncode=exit_code)

                with patch.object(check, 'snapshot', return_value=root), patch.object(check.subprocess, 'run', side_effect=process):
                    if blocked:
                        with self.assertRaises(RuntimeError):
                            check.run(root, 'probe')
                    else:
                        self.assertEqual(check.run(root, 'probe'), list(check.PROPERTIES[:1]) if broken else [])


if __name__ == '__main__':
    unittest.main()
