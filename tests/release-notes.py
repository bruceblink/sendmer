#!/usr/bin/env python3
"""Exercise release-note generation against an isolated Git history."""

from __future__ import annotations

import subprocess
import sys
import tempfile
from pathlib import Path


SCRIPT = (
    Path(__file__).resolve().parents[1]
    / ".github"
    / "scripts"
    / "generate_release_notes.py"
)
REPOSITORY = "https://github.com/example/sendmer"
RELEASE_DATE = "2026-08-22"


def run_git(repository: Path, *args: str) -> str:
    """Run one Git command in the fixture repository and return its output."""
    result = subprocess.run(
        ["git", "-c", "tag.gpgSign=false", *args],
        cwd=repository,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )
    return result.stdout.strip()


def commit(repository: Path, filename: str, subject: str) -> str:
    """Create one fixture commit and return its immutable commit identifier."""
    (repository / filename).write_text(subject + "\n", encoding="utf-8")
    run_git(repository, "add", filename)
    run_git(repository, "commit", "-m", subject)
    return run_git(repository, "rev-parse", "HEAD")


def assert_in_order(text: str, *markers: str) -> None:
    """Require each release section marker to appear after the preceding one."""
    positions = [text.index(marker) for marker in markers]
    if positions != sorted(positions):
        raise AssertionError(f"release sections are out of order: {markers}")


def generate(repository: Path, output: Path) -> subprocess.CompletedProcess[str]:
    """Generate notes for the fixture tag using a fixed date and repository URL."""
    return subprocess.run(
        [
            sys.executable,
            str(SCRIPT),
            "--tag",
            "v0.10.0",
            "--date",
            RELEASE_DATE,
            "--repository",
            REPOSITORY,
            "--output",
            str(output),
        ],
        cwd=repository,
        check=True,
        capture_output=True,
        text=True,
        encoding="utf-8",
    )


def main() -> None:
    """Build a tagged history, verify generated content, and reject invalid tags."""
    with tempfile.TemporaryDirectory(prefix="sendmer-release-notes-") as temporary:
        repository = Path(temporary)
        run_git(repository, "init", "-q")
        run_git(repository, "config", "user.email", "release-tests@example.invalid")
        run_git(repository, "config", "user.name", "sendmer release tests")

        seed = commit(repository, "seed.txt", "chore: seed")
        run_git(repository, "tag", "v0.9.0")
        docs = commit(repository, "docs.txt", "docs: add guide")
        feature = commit(repository, "feature.txt", "feat: add manifests")
        bugfix = commit(repository, "bugfix.txt", "fix(receiver): preserve metadata")
        other = commit(repository, "other.txt", "prototype change without a prefix")
        run_git(repository, "tag", "v0.10.0")

        first_output = repository / "release-notes.md"
        generate(repository, first_output)
        first_body = first_output.read_text(encoding="utf-8")
        second_output = repository / "release-notes-second.md"
        generate(repository, second_output)
        second_body = second_output.read_text(encoding="utf-8")

        if first_body != second_body:
            raise AssertionError("release notes changed between deterministic runs")
        if not first_body.startswith(f"## [0.10.0] - {RELEASE_DATE}\n"):
            raise AssertionError("release notes header did not use the tag and requested date")
        assert_in_order(
            first_body,
            "/ Features",
            "/ Bug Fixes",
            "/ Maintenance, Docs & Tests",
            "/ Other Changes",
        )
        for subject in (
            "add manifests",
            "preserve metadata",
            "add guide",
            "prototype change without a prefix",
        ):
            if subject not in first_body:
                raise AssertionError(f"missing generated change: {subject}")
        if "chore: seed" in first_body:
            raise AssertionError("release notes included a commit before the previous tag")
        if "/ Security" in first_body or "/ Performance" in first_body:
            raise AssertionError("empty release sections should be omitted")
        for commit_id in (docs, feature, bugfix, other):
            if f"{REPOSITORY}/commit/{commit_id}" not in first_body:
                raise AssertionError(f"missing source link for {commit_id}")
        if f"{REPOSITORY}/compare/v0.9.0...v0.10.0" not in first_body:
            raise AssertionError("missing full changelog comparison link")
        if seed in first_body:
            raise AssertionError("pre-release commit unexpectedly appeared in a source link")

        invalid_output = repository / "invalid.md"
        invalid = subprocess.run(
            [
                sys.executable,
                str(SCRIPT),
                "--tag",
                "not a release tag",
                "--output",
                str(invalid_output),
            ],
            cwd=repository,
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if invalid.returncode == 0:
            raise AssertionError("invalid release tags must be rejected")

    print("release notes contract passed")


if __name__ == "__main__":
    main()
