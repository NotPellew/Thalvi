#!/usr/bin/env python3
"""Each PR workflow ends in one `*-ok` job, the only check of it that branch protection
requires. It must wait for every other job of its workflow, so a job added later can't
merge while it fails."""

import re
import unittest
from pathlib import Path

WORKFLOWS = Path(__file__).resolve().parents[1] / "workflows"
# Each workflow's -ok job and the jobs it does not wait for, with the reason.
OK_JOBS = {
    # A failed detection makes every job run everything, so it fails nothing itself.
    "ci.yml": ("ci-ok", {"changes"}),
    "commits.yml": ("commits-ok", set()),
}


def jobs(text: str) -> dict[str, str]:
    """Each top-level job id with its lines, from a workflow indented by two spaces."""
    parts = re.split(
        r"^  ([A-Za-z0-9_-]+):[ \t]*\n",
        text.split("\njobs:\n", 1)[1],
        flags=re.MULTILINE,
    )
    return dict(zip(parts[1::2], parts[2::2], strict=True))


class OkJobs(unittest.TestCase):
    def test_each_ok_job_runs_always_and_waits_for_every_other_job(self) -> None:
        for name, (ok, exempt) in OK_JOBS.items():
            with self.subTest(workflow=name):
                found = jobs((WORKFLOWS / name).read_text())
                needs = re.search(r"^    needs: \[([^\]]*)\]", found[ok], re.MULTILINE)
                self.assertIsNotNone(needs, f"{ok} needs a one-line needs list")
                self.assertEqual(
                    {job.strip() for job in needs.group(1).split(",")},
                    set(found) - {ok} - exempt,
                )
                self.assertIn("    if: ${{ always() }}\n", found[ok])


if __name__ == "__main__":
    unittest.main()
