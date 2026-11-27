#!/usr/bin/env python3
"""Exercise the test-hygiene command against isolated tracked trees."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("test-hygiene")
UI = "crates/thalvi-app/ui/src"


def run_check(files: dict[str, str]) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        for name, text in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return subprocess.run([sys.executable, SCRIPT], cwd=root, capture_output=True, text=True)


def rust(*tests: str) -> str:
    return "".join(f"#[test]\nfn t{n}() {{\n{body}\n}}\n" for n, body in enumerate(tests))


class TestHygiene(unittest.TestCase):
    def test_a_test_file_must_be_named_elsewhere_or_sit_where_a_runner_collects_it(self):
        for files, finding in (
            ({"a/x.test.sh": "true\n"}, "a/x.test.sh:1"),
            ({"a/x.test.sh": "true\n", "a/gates": "a/x.test.sh\n"}, None),
            ({"a/x.test.sh": "true\n", "a/gates": "bx.test.sh\n"}, "a/x.test.sh:1"),
            ({"a/x.test.sh": "true\n", "NOTES.md": "x.test.sh\n"}, "a/x.test.sh:1"),
            ({"a/x.test.sh": "x.test.sh\n"}, "a/x.test.sh:1"),
            ({f"{UI}/deep/x.test.ts": "\n"}, None),
            ({"speech/asr/test_x.py": "\n"}, None),
            ({"speech/other/test_x.py": "\n"}, "speech/other/test_x.py:1"),
        ):
            with self.subTest(files=files):
                result = run_check(files)
                if finding is None:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                else:
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(finding, result.stdout)

    def test_a_rust_test_must_not_skip_itself_with_return(self):
        skip = 'if !available() { eprintln!("skipped"); return; }'
        for body, finding in (
            ("let Some(x) = maybe() else { return; };", "crates/a.rs:3"),
            (skip, "crates/a.rs:3"),
            ("return Ok(());", "crates/a.rs:3"),
            ("let x = match maybe() { Some(x) => x, None => return };", "crates/a.rs:3"),
            ("let f = |x: u8| { return x; };\nasync fn n() { return; }\nlet a = async { return 1; };\nf(1);", None),
            ('// return;\nlet s = "if x { return; }";', None),
            ("if child() {\n    assert!(works());\n    return;\n}\nrun();", None),
        ):
            with self.subTest(body=body):
                result = run_check({"crates/a.rs": rust(body)})
                if finding is None:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                else:
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(finding, result.stdout)

    def test_no_two_test_bodies_are_identical(self):
        for files, finding in (
            ({"crates/a.rs": "#[test]\nfn one() { check(1); /* c */ }\n#[tokio::test]\nasync fn two() {\n check(1);\n}\n"}, "crates/a.rs:4: same body as crates/a.rs:2 one"),
            ({"crates/a.rs": rust("check(1);", "check(2);", 'check("1");')}, None),
            ({f"{UI}/a.test.ts": 'it("one", () => {\n  run(1); // c\n});\ntest("two", async () => { run(1); });\n'}, f"{UI}/a.test.ts:4: same body as {UI}/a.test.ts:1 one"),
            ({f"{UI}/a.test.ts": 'it("one", () => { run("a"); });\nit("two", () => { run("b"); });\n'}, None),
        ):
            with self.subTest(files=files):
                result = run_check(files)
                if finding is None:
                    self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
                else:
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertIn(finding, result.stdout)


if __name__ == "__main__":
    unittest.main()
