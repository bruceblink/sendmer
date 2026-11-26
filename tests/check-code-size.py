#!/usr/bin/env python3
"""Regression tests for the code size gate, independent of the Rust build."""

import importlib.util
import tempfile
import subprocess
import sys
import unittest
from pathlib import Path

CHECKER = Path(__file__).resolve().parents[1] / "scripts" / "check-code-size.py"
SPEC = importlib.util.spec_from_file_location("check_code_size", CHECKER)
checker = importlib.util.module_from_spec(SPEC)
sys.dont_write_bytecode = True
SPEC.loader.exec_module(checker)


class CodeSizeTests(unittest.TestCase):
    def test_file_boundaries(self):
        self.assertEqual(list(checker.violations("test.rs", b"// line\n" * 600)), [])
        errors = list(checker.violations("test.rs", b"// line\n" * 600 + b"// last"))
        self.assertEqual(len(errors), 1)
        self.assertIn("601 file lines", errors[0])

    def test_function_boundaries(self):
        for path, opening, comment, closing in [
            ("test.rs", "async fn check() {", "    // }", "}"),
            ("test.sh", "check() {", "    # }", "}"),
            ("test.ps1", "function Check {", "    # }", "}"),
            ("test.py", "async def check():", "    # }", "    pass"),
        ]:
            with self.subTest(path=path):
                source = lambda length: "\n".join([opening] + [comment] * (length - 2) + [closing]).encode()
                self.assertEqual(list(checker.violations(path, source(120))), [])
                errors = list(checker.violations(path, source(121)))
                self.assertEqual(len(errors), 1, errors)
                self.assertIn("121 function lines", errors[0])

    def test_nested_functions_and_literals(self):
        source = b'fn outer() {\n    let _ = r#" } { fn fake() {} "#;\n    fn inner() {\n' + b"        // }\n" * 119 + b"    }\n}"
        errors = list(checker.violations("test.rs", source))
        self.assertEqual(len(errors), 2, errors)
        self.assertTrue(any("inner: 121 function lines" in error for error in errors))
        self.assertTrue(any("outer:" in error for error in errors))

    def test_invalid_syntax_fails_closed(self):
        for path, source in [("test.rs", b"fn broken( {"), ("test.py", b"def broken(")]:
            self.assertTrue(any("cannot check" in error for error in checker.violations(path, source)))

    def test_source_discovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            subprocess.run(["git", "init", "-q", str(root)], check=True)
            (root / ".gitignore").write_text("target/\n")
            (root / "tracked.rs").touch()
            (root / "new.sh").touch()
            (root / "README.md").touch()
            (root / "target").mkdir()
            (root / "target" / "ignored.rs").touch()
            subprocess.run(["git", "add", "tracked.rs", ".gitignore"], cwd=root, check=True)
            self.assertEqual([p.name for p in checker.source_files(root)], ["new.sh", "tracked.rs"])


if __name__ == "__main__":
    unittest.main()
