#!/usr/bin/env python3
"""Print a branch's changed lines as production code and tests (#2355).

AGENTS.md caps a pull request's production code and its tests separately, so
a PR that carries the negative cases and revert witnesses this repository
requires is not split for them. Counting both in one number is what the
separate caps exist to avoid, and a count by file name alone would book every
inline `#[cfg(test)]` module as production code; this script draws the line
inside the file.

Usage: python3 scripts/pr-size.py [BASE]   (default: origin/master)

Changed lines are additions plus deletions against the merge base. `docs/`
and Markdown count toward neither budget.

Test regions inside a Rust source file are top-level `#[cfg(test)]` modules
as rustfmt lays them out: the attribute and `mod name {` at column 0, closed
by a `}` at column 0. Lines inside string literals (raw ones included) and
block comments never open a region, so a test fixture that spells out a
module does not turn the code after it into tests.
"""

import re
import subprocess
import sys

TEST_PATH = re.compile(
    r"(^|/)tests?/"
    r"|(^|/)tests\.rs$"
    r"|_tests?\.rs$"
    r"|(^|/)fixtures/"
    r"|(^|/)test_support"
    r"|\.golden$"
    r"|^scripts/[^/]*_test\.py$"
)
CFG_TEST = re.compile(r"#\[cfg\((all\()?test\b")
MOD_OPEN = re.compile(r"(pub(\([^)]*\))? )?mod \w+ \{$")


def category(path):
    """'docs', 'tests' or 'production' for a whole file, or 'rust' when the
    answer depends on the line."""
    if path.startswith("docs/") or path.endswith(".md"):
        return "docs"
    if TEST_PATH.search(path):
        return "tests"
    if path.endswith(".rs"):
        return "rust"
    return "production"


def code_starts(text):
    """For each line, whether it starts outside any string or block comment."""
    starts = []
    state = None  # None, '"', ('r', hashes) or ('/*', depth)
    i, n = 0, len(text)
    starts.append(True)
    while i < n:
        c = text[i]
        if c == "\n":
            starts.append(state is None)
            i += 1
            continue
        if state is None:
            if text.startswith("//", i):
                j = text.find("\n", i)
                i = n if j < 0 else j
                continue
            if text.startswith("/*", i):
                state, i = ("/*", 1), i + 2
                continue
            m = re.match(r"b?r(#*)\"", text[i:])
            if m and (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")):
                state, i = ("r", len(m.group(1))), i + m.end()
                continue
            if c == "'":
                # A char literal ('"', '\'', 'x') or a lifetime ('a): skip the
                # literal so its quote cannot open a string.
                m = re.match(r"'(\\.|[^\\'\n])'", text[i:])
                i += m.end() if m else 1
                continue
            if c == '"':
                state, i = '"', i + 1
                continue
            i += 1
        elif state == '"':
            if c == "\\":
                # A line continuation still ends a line, inside the string.
                if text.startswith("\n", i + 1):
                    starts.append(False)
                i += 2
                continue
            if c == '"':
                state = None
            i += 1
        elif state[0] == "r":
            close = '"' + "#" * state[1]
            if text.startswith(close, i):
                state, i = None, i + len(close)
                continue
            i += 1
        else:
            if text.startswith("*/", i):
                depth = state[1] - 1
                state, i = (None if depth == 0 else ("/*", depth)), i + 2
                continue
            if text.startswith("/*", i):
                state, i = ("/*", state[1] + 1), i + 2
                continue
            i += 1
    return starts


def test_lines(text):
    """The 1-based numbers of the lines inside top-level test modules."""
    lines = text.split("\n")
    starts = code_starts(text)
    inside, pending, found = False, False, set()
    for number, (line, code) in enumerate(zip(lines, starts), 1):
        if inside:
            found.add(number)
            if line == "}" and code:
                inside = False
            continue
        if not code:
            pending = False
            continue
        if CFG_TEST.match(line):
            pending = True
            found.add(number)
        elif pending and line.startswith("#["):
            found.add(number)
        elif pending and MOD_OPEN.match(line):
            inside = True
            found.add(number)
            pending = False
        else:
            pending = False
    return found


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout


def changed(base, head):
    """Yield (path, side, line number) for every changed line; side is '-'
    for a deletion (old file) and '+' for an addition (new file)."""
    diff = git("diff", "-U0", "--no-color", "--no-renames", base, head)
    path = None
    hunk = re.compile(r"@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@")
    old = new = 0
    # `---`/`+++` name the file only in the header before the first hunk: a
    # deleted SQL comment line also reads `--- ...`.
    header = False
    for line in diff.split("\n"):
        if line.startswith("diff --git "):
            path, header = None, True
        elif header and line.startswith("--- "):
            if line != "--- /dev/null":
                path = line[6:]
        elif header and line.startswith("+++ "):
            if line != "+++ /dev/null":
                path = line[6:]
        elif line.startswith("@@"):
            header = False
            m = hunk.match(line)
            old, new = int(m.group(1)), int(m.group(2))
        elif line.startswith("-") and path:
            yield path, "-", old
            old += 1
        elif line.startswith("+") and path:
            yield path, "+", new
            new += 1


def measure(base, head="HEAD"):
    base = git("merge-base", base, head).strip()
    totals = {"production": 0, "tests": 0, "docs": 0}
    regions = {}
    for path, side, number in changed(base, head):
        kind = category(path)
        if kind == "rust":
            key = (path, side)
            if key not in regions:
                rev = base if side == "-" else head
                regions[key] = test_lines(git("show", f"{rev}:{path}"))
            kind = "tests" if number in regions[key] else "production"
        totals[kind] += 1
    return totals


def main():
    base = sys.argv[1] if len(sys.argv) > 1 else "origin/master"
    totals = measure(base)
    for kind in ("production", "tests", "docs"):
        print(f"{kind}: {totals[kind]}")


if __name__ == "__main__":
    main()
