#!/usr/bin/env python3
"""Exercise the lint-exceptions command against isolated Rust sources and budgets."""

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().with_name("lint-exceptions")
BUDGET = ".github/scripts/lint-exceptions.toml"


def run_check(budget: str, **sources: str) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        files = {BUDGET: budget, **{f"crates/{name}.rs": text for name, text in sources.items()}}
        for name, text in files.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text, encoding="utf-8")
        return subprocess.run([sys.executable, SCRIPT], cwd=root, capture_output=True, text=True)


class LintExceptions(unittest.TestCase):
    def test_counts_each_lint_in_inner_outer_and_nested_attributes(self):
        result = run_check(
            '"clippy::too_many_lines" = 2\n"clippy::unused_self" = 1\n"unsafe_code" = 1\n',
            demo='''\
#![expect(unsafe_code, reason = "platform: ffi")]
#[cfg_attr(not(test), r#cfg_attr(feature = "demo", r#expect(
    clippy::too_many_lines, clippy::unused_self, reason = "both",
)))]
fn nested() {}
#[expect(clippy::too_many_lines, reason = "one")]
fn outer() {}
''',
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_rust_whitespace_can_separate_attribute_tokens(self):
        result = run_check(
            '"dead_code" = 1\n', demo=' #\u200e!\u200f[expect\u200e(dead_code, reason = "platform: test")]\n'
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_module_wide_exceptions_need_a_platform_reason(self):
        for source in (
            '#![{attribute}]\nfn item() {{}}',
            '#![cfg_attr(unix, cfg_attr(all(), {attribute}))]\nfn item() {{}}',
            '#[{attribute}]\n#[cfg(unix)]\n/* comment */ pub(crate) mod demo;',
            '#[cfg_attr(unix, {attribute})]\n/// docs\npub mod/* layout */demo {{}}',
        ):
            for reason, accepted in (
                (', reason = "platform: FFI"', True),
                (', r#reason = r#"platform: FFI"#', True),
                (r', reason = "platform\x3a FFI"', True),
                (', reason = "FFI"', False),
                (', reason = "needs platform: FFI"', False),
                ('', False),
            ):
                with self.subTest(source=source, reason=reason):
                    attribute = f'expect(unsafe_code{reason})'
                    result = run_check('"unsafe_code" = 1\n', demo=source.format(attribute=attribute))
                    self.assertEqual(result.returncode, int(not accepted), result.stdout + result.stderr)
                    if not accepted:
                        self.assertIn('crates/demo.rs:1', result.stdout)
                        self.assertIn('platform:', result.stdout)

    def test_a_field_whose_name_starts_with_mod_is_not_a_module(self):
        result = run_check(
            '"dead_code" = 1\n',
            demo='''\
pub struct Holder {
    #[expect(dead_code, reason = "only the field")]
    modération: i32,
}
''',
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_each_expect_in_a_module_cfg_attr_needs_its_own_reason(self):
        result = run_check(
            '"unsafe_code" = 1\n"dead_code" = 1\n',
            demo='''\
#[cfg_attr(unix,
    expect(unsafe_code, reason = "platform: FFI"),
    expect(dead_code, reason = "unused here"),
)]
mod demo;
''',
        )
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertNotIn('crates/demo.rs:2', result.stdout)
        self.assertIn('crates/demo.rs:3', result.stdout)

    def test_deep_cfg_attr_nesting_applies_the_same_module_rule(self):
        for reason, code in (("platform: FFI", 0), ("FFI", 1)):
            with self.subTest(reason=reason):
                leaf = f'expect(unsafe_code, reason = "{reason}")'
                attribute = 'cfg_attr(all(), ' * 520 + leaf + ')' * 520
                result = run_check(
                    '"unsafe_code" = 1\n',
                    demo=f'#![recursion_limit = "4096"]\n#[{attribute}]\nmod demo {{}}',
                )
                self.assertEqual(result.returncode, code, result.stdout + result.stderr)
                if code:
                    self.assertIn('crates/demo.rs:2', result.stdout)
                    self.assertIn('platform:', result.stdout)

    def test_ignores_comments_strings_and_the_cfg_condition(self):
        result = run_check(
            '"dead_code" = 1\n',
            demo=r'''
// #[expect(unused_imports)]
/* outer /* #[expect(unsafe_code)] */ #[expect(unused_variables)] */
const COOKED: &str = "\" #[expect(unused_imports)]";
const BYTES: &[u8] = b"#[expect(unused_imports)]";
const RAW: &str = r##" #[expect(unsafe_code, reason = "\"#")] "##;
const RAW_BYTES: &[u8] = br#"#[expect(unused_variables)]"#;
const C_STRING: &core::ffi::CStr = cr#"#[expect(unused_variables)]"#;
#[cfg_attr(expect = "not an attribute", expect(dead_code, reason = "real"))]
fn real<'a>(_: &'a str) {}
''',
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_over_budget_lists_every_site_and_treats_unlisted_lints_as_zero(self):
        result = run_check(
            '"clippy::redundant_clone" = 1\n',
            first='#[expect(clippy::redundant_clone, unused_variables, reason = "test")]\nfn a() {}\n',
            second='\n#[cfg_attr(all(), r#expect(\n    clippy::redundant_clone, reason = "test",\n))]\nfn b() {}\n',
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("clippy::redundant_clone has 2, budget is 1", result.stdout)
        self.assertIn("unused_variables has 1, budget is 0", result.stdout)
        self.assertIn("crates/first.rs:1", result.stdout)
        self.assertIn("crates/second.rs:3", result.stdout)

    def test_under_budget_requests_the_exact_lower_count(self):
        result = run_check('"clippy::redundant_clone" = 1\n', demo="fn demo() {}\n")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn(f"lower the budget for clippy::redundant_clone to 0 in {BUDGET}", result.stdout)

    def test_rejects_malformed_or_invalid_budgets(self):
        for budget, message in (
            ('"dead_code" = [\n', "is not valid TOML"),
            ('"dead_code" = true\n', "must be a non-negative integer"),
            ('"dead_code" = -1\n', "must be a non-negative integer"),
        ):
            with self.subTest(budget=budget):
                result = run_check(budget)
                self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
                self.assertIn(BUDGET, result.stderr)
                self.assertIn(message, result.stderr)


if __name__ == "__main__":
    unittest.main()
