#!/usr/bin/env python3
"""Exercise the hold command with paginated, data-only GitHub responses."""

import copy
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPTS = Path(__file__).resolve().parent
REPO = "NotPellew/Thalvi"
PR = "pulls/23"
HEAD = "a" * 40
BASE = "b" * 40
OWNER = "NotPellew"
AGENT = "app/notpellew-agents"
RUNS = "actions/workflows/hold.yml/runs?per_page=100"
PULLS = "pulls?state=open&per_page=100"
FILES = f"{PR}/files?per_page=100"
EVENTS = "issues/23/events?per_page=100"
SOURCE = {"filename": "crates/thalvi-app/src/main.rs", "status": "modified"}

# The fixture also records successful rerun requests as observable effects.
FAKE_GH = """#!/usr/bin/env python3
import json
import os
from pathlib import Path
import sys

args = sys.argv[1:]
endpoint = args[-1].removeprefix("repos/NotPellew/Thalvi/")
state = Path(os.environ["HOLD_FIXTURE"])
data = json.loads(state.read_text())
if args[0] != "api" or endpoint not in data or not data[endpoint]:
    sys.exit(f"unexpected API request: {args!r}")
replies = data[endpoint]
reply = replies.pop(0) if len(replies) > 1 else replies[0]
state.write_text(json.dumps(data))
if reply == "partial API error":
    print("[[")
    if "--paginate" not in args:
        sys.exit(0)
if reply in ("API error", "partial API error"):
    sys.exit("fixture API error")
if args[1:3] == ["--method", "POST"]:
    with Path(os.environ["HOLD_POSTS"]).open("a") as posts:
        print(endpoint, file=posts)
elif "?per_page=100" in endpoint or "&per_page=100" in endpoint:
    if "--paginate" not in args:
        print(json.dumps(reply[0]))
    elif "--slurp" not in args:
        for page in reply:
            print(json.dumps(page))
    else:
        print(json.dumps(reply))
else:
    print(json.dumps(reply))
"""


def label(name):
    return {"name": name}


def issue(title="feat(desktop): a feature", *labels):
    return {"title": title, "labels": list(map(label, labels))}


def transition(event, actor, id_):
    return {
        "event": event,
        "actor": {"login": actor},
        "label": label("owner-ok"),
        "created_at": "2026-11-25T10:00:00Z",
        "id": id_,
    }


def run(id_, number, status="completed", event="pull_request_target", head=HEAD, base="main"):
    return {
        "id": id_,
        "run_number": id_,
        "display_title": f"Hold PR #{number} {head} into {base}",
        "head_branch": "main",
        "event": event,
        "status": status,
        "pull_requests": [],
    }


class HoldTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name)
        fake = self.directory / "gh"
        fake.write_text(FAKE_GH)
        fake.chmod(0o755)
        self.summary = self.directory / "summary"
        self.posts = self.directory / "posts"
        self.pr = {
            "number": 23,
            "user": {"login": AGENT},
            "body": "Closes #17",
            "draft": True,
            "labels": [],
            "changed_files": 1,
            "head": {"sha": HEAD},
            "base": {"sha": BASE, "ref": "main"},
            "state": "open",
            "updated_at": "2026-11-25T10:00:00Z",
        }
        self.data = {PR: [self.pr], FILES: [[[SOURCE]]], "issues/17": [issue("title", "bug")]}

    def invoke(self, *args):
        state = self.directory / "fixture.json"
        state.write_text(json.dumps(self.data))
        self.summary.unlink(missing_ok=True)
        self.posts.unlink(missing_ok=True)
        env = {
            "PATH": f"{self.directory}{os.pathsep}{os.environ['PATH']}",
            "GITHUB_REPOSITORY": REPO,
            "GITHUB_STEP_SUMMARY": str(self.summary),
            "HOLD_FIXTURE": str(state),
            "HOLD_POSTS": str(self.posts),
            "HOME": str(self.directory),
        }
        return subprocess.run(
            [str(SCRIPTS / "hold"), *args], cwd=SCRIPTS, env=env,
            text=True, capture_output=True, timeout=30, check=False,
        )

    def check(self, code, *messages):
        result = self.invoke("check", "23", HEAD, "main")
        self.assertEqual(result.returncode, code, result.stderr)
        summary = self.summary.read_text()
        for message in messages:
            self.assertIn(message, summary)
        actual_sha = subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=SCRIPTS, text=True,
        ).strip()
        self.assertEqual(summary.splitlines()[-1], f"Policy commit: {actual_sha}")
        return result

    def test_draft_source_fix_passes_by_each_accepted_label_and_title(self):
        for candidate in (issue("title", "bug"), issue("title", "ci-regression"),
                          issue("fix(desktop): audio"), issue("fix: audio")):
            with self.subTest(candidate=candidate):
                self.data["issues/17"] = [candidate]
                self.check(0, "hold: passed", "#17", "protected paths")

    def test_reports_every_issue_and_path_reason(self):
        self.pr["body"] = "Closes #17, fixes #18, closes NotPellew/Thalvi#19"
        self.data["issues/18"] = [issue("feat(desktop): a feature")]
        self.data["issues/19"] = [issue("chore(ci): routine work")]
        self.data[FILES] = [[[{"filename": "Cargo.lock", "status": "modified"}]]]
        self.check(1, "#18 is a feature", "#19", "chore", "Cargo.lock")

    def test_body_without_a_local_closing_reference_holds(self):
        for body in (None, "Refs #17", "Closes elsewhere/Thalvi#17", "<!-- Closes #17 -->"):
            with self.subTest(body=body):
                self.pr["body"] = body
                self.check(1, "closes no issue")

    def test_pull_request_is_not_a_qualifying_issue(self):
        self.data["issues/17"] = [dict(issue("fix: audio", "bug"), pull_request={})]
        self.check(1, "#17 is a pull request")

    # High risk: #693
    def test_policy_paths_hold_and_renames_check_both_names_across_pages(self):
        paths = [
            ".github/scripts/hold", ".github/workflows/hold.yml", ".githooks/pre-commit",
            ".cargo/config.toml", "DECISIONS.md", "AGENTS.md", "CLAUDE.md",
            ".agents/skills/verify-thalvi/SKILL.md", "Cargo.toml", "crates/audio/Cargo.toml",
            "Cargo.lock", "package.json", "crates/app/ui/package.json", "package-lock.json",
            "crates/app/ui/package-lock.json", "deny.toml", "rust-toolchain.toml",
            "clippy.toml", ".gitleaks.toml", ".node-version", ".python-version",
        ]
        files = [{"filename": path, "status": "added"} for path in paths[:-2]]
        files.extend((
            {"filename": "node.txt", "previous_filename": paths[-2], "status": "renamed"},
            {"filename": paths[-1], "previous_filename": "python.txt", "status": "renamed"},
        ))
        self.pr["changed_files"] = len(files)
        self.data[FILES] = [[files[:1], files[1:]]]
        self.check(1, *paths)

    def test_owner_author_passes_with_no_closing_reference(self):
        self.pr.update(user={"login": OWNER}, body=None)
        self.check(0, "owner authored")

    # High risk: #693
    def test_only_the_actor_who_added_the_current_owner_label_can_release(self):
        self.pr["labels"] = [label("owner-ok")]
        self.pr["body"] = None
        added_by_owner = transition("labeled", OWNER, 10)
        removed = transition("unlabeled", OWNER, 11)
        added_by_agent = transition("labeled", AGENT, 12)
        self.data[EVENTS] = [[[added_by_owner]]]
        self.check(0, "owner approved")
        self.data[EVENTS] = [[[added_by_agent], [removed, added_by_owner]]]
        self.check(1, "owner-ok", AGENT, "closes no issue")
        self.data[EVENTS] = [[[added_by_owner, removed], [added_by_agent,
                             transition("unlabeled", AGENT, 13), transition("labeled", OWNER, 14)]]]
        self.check(0, "owner approved")
        self.data[EVENTS] = [[[added_by_owner, removed]]]
        self.check(1, "owner-ok", "closes no issue")
        del self.data[EVENTS]
        self.pr["labels"] = []
        self.check(1, "closes no issue")

    def test_untrusted_owner_label_does_not_disable_bug_fix_rule(self):
        self.pr["labels"] = [label("owner-ok")]
        self.data[EVENTS] = [[[transition("labeled", AGENT, 12)]]]
        self.check(0, "hold: passed", "owner-ok", AGENT)

    def test_api_failures_fail_the_check_even_after_one_file_page(self):
        for endpoint in (PR, "issues/17", FILES, EVENTS):
            with self.subTest(endpoint=endpoint):
                before = copy.deepcopy(self.data)
                if endpoint == EVENTS:
                    self.pr["labels"] = [label("owner-ok")]
                self.data[endpoint] = ["partial API error" if endpoint == FILES else "API error"]
                result = self.check(1, "hold: error")
                self.assertIn("fixture API error", result.stderr)
                self.data = before
                self.pr = self.data[PR][0]

    def test_incomplete_file_lists_cannot_pass(self):
        for pages, count in (
            ([[SOURCE]], 2), ([[SOURCE], [SOURCE]], 2), ([[SOURCE]], 3001),
            ([[{"filename": SOURCE["filename"], "status": "renamed"}]], 1),
        ):
            with self.subTest(pages=pages, count=count):
                self.pr["changed_files"] = count
                self.data[FILES] = [pages]
                self.check(1, "changed file list")

    def test_head_or_base_change_cannot_pass_an_earlier_run(self):
        self.data[PR] = [self.pr, dict(self.pr, head={"sha": "c" * 40})]
        self.check(1, "PR changed")
        self.data[PR] = [dict(self.pr, base={"sha": BASE, "ref": "other"})]
        self.check(1, "PR changed")

    def set_open_prs(self):
        def record(number, body, source_repo):
            return {
                "number": number, "body": body, "base": {"ref": "main"},
                "head": {"sha": HEAD, "ref": "same", "repo": {"full_name": source_repo}},
            }
        self.data[PULLS] = [[
            [record(23, "Closes #17", "fork/Thalvi")],
            [record(24, "Refs #17", REPO), record(123, "Closes #17", REPO)],
        ]]

    def test_issue_event_reruns_only_latest_run_for_each_matching_pr(self):
        self.set_open_prs()
        self.data[RUNS] = [[
            {"workflow_runs": [run(793, 24), run(791, 23)]},
            {"workflow_runs": [run(794, 123), run(790, 23),
                               run(799, 23, event="issues")]},
        ]]
        expected = ["actions/runs/791/rerun", "actions/runs/794/rerun"]
        self.data.update({endpoint: [None] for endpoint in expected})
        result = self.invoke("rerun", "17")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertCountEqual(self.posts.read_text().splitlines(), expected)

    def test_active_newer_run_is_never_bypassed_by_older_completed_one(self):
        self.set_open_prs()
        self.data[PULLS][0] = self.data[PULLS][0][:1]
        self.data[RUNS] = [
            [{"workflow_runs": [run(790, 23, head="c" * 40), run(789, 23, base="other")]}],
            [{"workflow_runs": [run(790, 23), run(791, 23, "in_progress")]}],
            [{"workflow_runs": [run(791, 23), run(792, 23)]}],
        ]
        self.data["actions/runs/792/rerun"] = [None]
        result = self.invoke("rerun", "17")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.posts.read_text().splitlines(), ["actions/runs/792/rerun"])

    def test_issue_event_api_errors_fail_without_claiming_rerun_success(self):
        self.set_open_prs()
        self.data[PULLS][0] = self.data[PULLS][0][:1]
        self.data[RUNS] = [[{"workflow_runs": [run(791, 23)]}]]
        for endpoint in (PULLS, RUNS, "actions/runs/791/rerun"):
            with self.subTest(endpoint=endpoint):
                before = copy.deepcopy(self.data)
                self.data[endpoint] = ["API error"]
                result = self.invoke("rerun", "17")
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("fixture API error", result.stderr)
                self.assertFalse(self.posts.exists())
                self.data = before


if __name__ == "__main__":
    unittest.main()
