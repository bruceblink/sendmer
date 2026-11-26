#!/usr/bin/env python3
"""Enforce physical file/function line limits for repository source and tests."""

import ast
import subprocess
import sys
from pathlib import Path

from tree_sitter import Language, Parser
import tree_sitter_bash
import tree_sitter_powershell
import tree_sitter_rust

FILE_LIMIT = 600
FUNCTION_LIMIT = 120
GRAMMARS = {
    ".rs": (tree_sitter_rust, "function_item"),
    ".sh": (tree_sitter_bash, "function_definition"),
    ".ps1": (tree_sitter_powershell, "function_statement"),
}


def source_files(root):
    """Include tracked and new, non-ignored files; omit removed files."""
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=root,
    )
    return sorted(
        path for path in {root / name.decode("utf-8") for name in paths.split(b"\0") if name}
        if path.suffix.lower() in {*GRAMMARS, ".py"} and path.is_file()
    )


def nodes(node):
    """Visit nested functions as well as top-level functions."""
    stack = [node]
    while stack:
        current = stack.pop()
        yield current
        stack.extend(reversed(current.children))


def tree_functions(source, suffix):
    grammar, kind = GRAMMARS[suffix]
    parser = Parser(Language(grammar.language()))
    root = parser.parse(source).root_node
    for node in nodes(root):
        if node.is_error or node.is_missing:
            raise ValueError(f"syntax parse error at line {node.start_point.row + 1}")
        if node.type == kind:
            name = node.child_by_field_name("name")
            if name is None:
                # The PowerShell grammar exposes function_name as a named child.
                name = next((child for child in node.named_children if child.type == "function_name"), None)
            if name is None:
                raise ValueError(f"unrecognized function name at line {node.start_point.row + 1}")
            yield name.text.decode("utf-8"), node.start_point.row + 1, node.end_point.row + 1


def python_functions(source):
    for node in ast.walk(ast.parse(source)):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            yield node.name, node.lineno, node.end_lineno


def violations(path, source):
    # Physical lines include comments and blank lines, and the last line even
    # when it has no trailing newline. A function includes its signature and
    # body (through its closing brace), not attributes/decorators before it.
    count = len(source.splitlines())
    if count > FILE_LIMIT:
        yield f"{path}: {count} file lines (limit {FILE_LIMIT})"

    suffix = Path(path).suffix.lower()
    try:
        functions = python_functions(source) if suffix == ".py" else tree_functions(source, suffix)
        for name, start, end in functions:
            count = end - start + 1
            if count > FUNCTION_LIMIT:
                yield f"{path}:{start}: {name}: {count} function lines (limit {FUNCTION_LIMIT})"
    except (SyntaxError, ValueError) as error:
        yield f"{path}: cannot check function sizes: {error}"


def main():
    root = Path(__file__).resolve().parent.parent
    files = source_files(root)
    errors = [
        error
        for path in files
        for error in violations(path.relative_to(root).as_posix(), path.read_bytes())
    ]
    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 1
    print(f"Code size check passed: {len(files)} files; limits {FILE_LIMIT}/{FUNCTION_LIMIT}.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
