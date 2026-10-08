#!/usr/bin/env python3
"""Check the decision record's identifiers and every citation of them.

The record lives in docs/decisions/, one file per topic, indexed by
docs/DECISIONS.md (issue #671). Entries 1..N were numbered in one sequence while
the record was one file; that sequence is closed, and a new entry is named
after its issue, DEC-<issue>.<k>. Two branches that each took "the next number"
would now write it into two different files, where git reports no conflict and
the duplicate would merge unseen. This check is what makes that loud.

It refuses:
  - an identifier that two entries claim;
  - an entry numbered past the closed sequence's fixed end, even with an index
    row for it (a stale branch appending "N+1."), one the index does not list,
    or one the index places in another file;
  - an entry without the anchor its citations link to, or with the wrong one;
  - a number of the closed sequence whose entry or index row is gone — the
    sequence is complete except for the four numbers it never used (523–526),
    and those may not be taken now;
  - a citation, `DECISIONS <n>` or `DEC-<issue>.<k>`, that names no entry —
    every number of a grouped one (`DECISIONS 70, 87 and 90`, `181–190`). One
    of the four unused numbers is accepted: branches reserved them, and
    records still mention them as proposals.
"""

import re
import subprocess
import sys
from pathlib import Path

INDEX = "docs/DECISIONS.md"
TOPICS = "docs/decisions/"
# Files that spell identifiers as examples or fixtures, not as citations.
NOT_CITATIONS = {INDEX, "scripts/check-decisions.py", "scripts/check_decisions_test.py"}

LEGACY_ENTRY = re.compile(r"^(\d+)\. \*\*")
NEW_ENTRY = re.compile(r"^\*\*DEC-(\d+)\.(\d+)\. ")
ANCHOR = re.compile(r'^<a id="(decision-(\d+)|dec-(\d+)-(\d+))"></a>$')
INDEX_ROW = re.compile(r"^\| (\d+) \| \[[^\]]+\]\(decisions/([^)#]+)#decision-(\d+)\) \|")
# The last number of the closed sequence. Fixed here rather than read from the
# index, which is an ordinary file a branch could extend along with its entry.
CLOSED = 543
# The numbers the closed sequence never used: branches reserved them and the
# record still mentions them as proposals (see the index). Fixed here rather
# than read as "whatever the index does not list", because then deleting an
# entry together with its index row would turn its number into one of these,
# and every citation of the lost decision would pass (#848).
RESERVED = frozenset({523, 524, 525, 526})

# One legacy citation group: a number or a range, then more of them after a
# comma, `and` or `, and`. A separator is taken only when a number follows, so
# `DECISIONS 267, and the difference` and `DECISIONS 311, DEC-942.1` end at the
# first number; a dash with no number after it ends the group too.
ITEM = r"\d+(?:[ \t]*[-–][ \t]*\d+)?"
CITE_LEGACY = re.compile(
    rf"\bDECISIONS ({ITEM}(?:(?:,[ \t]*(?:and[ \t]+)?|[ \t]+and[ \t]+){ITEM})*)\b"
)
CITE_ITEM = re.compile(r"(\d+)(?:[ \t]*[-–][ \t]*(\d+))?")
CITE_NEW = re.compile(r"\bDEC-(\d+)\.(\d+)\b")


def entries_of(path, text):
    """Yield (identifier, line number, problem-or-None) for each entry in a topic file."""
    lines = text.split("\n")
    for i, line in enumerate(lines):
        legacy = LEGACY_ENTRY.match(line)
        new = NEW_ENTRY.match(line)
        if not legacy and not new:
            continue
        ident = legacy[1] if legacy else f"DEC-{new[1]}.{new[2]}"
        want = f"decision-{legacy[1]}" if legacy else f"dec-{new[1]}-{new[2]}"
        # The anchor sits two lines up, a blank line between: it has to end
        # the previous list, or the renderer numbers this entry as the next
        # item of that list rather than as itself.
        above = lines[i - 2] if i >= 2 else ""
        anchor = ANCHOR.match(above)
        problem = None
        if not anchor or anchor[1] != want or lines[i - 1].strip():
            problem = f'needs `<a id="{want}"></a>` and a blank line directly above it'
        yield ident, i + 1, problem


def cited_numbers(group, closed):
    """The numbers one citation group names, and any problem with its shape.

    A range names both endpoints and everything between. Only the part inside
    the closed sequence is listed one by one; past it, the range's end alone is
    named, so a typo of `1–900000000` reports one number, not most of them.
    """
    numbers, problems = [], []
    for item in CITE_ITEM.finditer(group):
        low = int(item[1])
        high = int(item[2]) if item[2] else low
        if high < low:
            problems.append(f"DECISIONS {item[0]} is a range that runs backwards")
            continue
        numbers.extend(range(low, min(high, closed) + 1))
        if high > closed:
            numbers.append(high if low <= closed else low)
    return numbers, problems


def check(topic_files, index_text, cited_in, closed=CLOSED):
    """topic_files: {path: text}; cited_in: {path: text}. Returns a list of errors."""
    errors = []
    where = {}
    for path, text in sorted(topic_files.items()):
        for ident, line, problem in entries_of(path, text):
            if problem:
                errors.append(f"{path}:{line}: {ident} {problem}")
            if ident in where:
                errors.append(f"{path}:{line}: {ident} is already {where[ident]}")
            else:
                where[ident] = f"{path}:{line}"

    indexed = {}
    for line in index_text.split("\n"):
        row = INDEX_ROW.match(line)
        if row:
            if row[1] != row[3]:
                errors.append(f"{INDEX}: row {row[1]} links to decision-{row[3]}")
            indexed[row[1]] = TOPICS + row[2]
    legacy = {i: w for i, w in where.items() if not i.startswith("DEC-")}
    reserved = {n for n in RESERVED if n <= closed}
    for ident, at in sorted(legacy.items(), key=lambda x: int(x[0])):
        path = at.rsplit(":", 1)[0]
        if int(ident) > closed or int(ident) in reserved:
            errors.append(
                f"{at}: {ident} is a new number in the closed sequence; "
                f"name the entry DEC-<issue>.<k> instead (see {INDEX})"
            )
        elif ident not in indexed:
            errors.append(f"{at}: {ident} has no row in {INDEX}")
        elif indexed[ident] != path:
            errors.append(f"{at}: {INDEX} places {ident} in {indexed[ident]}")
    for ident, path in indexed.items():
        if int(ident) > closed:
            errors.append(f"{INDEX}: row {ident} extends the closed sequence, which ends at {closed}")
        elif int(ident) in reserved:
            errors.append(f"{INDEX}: row {ident} takes a number the closed sequence never used")
        elif ident not in legacy:
            errors.append(f"{INDEX}: {ident} is indexed but no entry in {path} has it")
    # Every number the sequence used is still there. An entry deleted with its
    # index row leaves neither of the checks above anything to compare.
    for n in range(1, closed + 1):
        if n not in reserved and str(n) not in legacy and str(n) not in indexed:
            errors.append(f"{INDEX}: {n} has neither an entry nor an index row; it was lost")

    for path, text in sorted(cited_in.items()):
        if path in NOT_CITATIONS:
            continue
        for number, line in enumerate(text.split("\n"), 1):
            for group in CITE_LEGACY.findall(line):
                numbers, problems = cited_numbers(group, closed)
                errors.extend(f"{path}:{number}: {p}" for p in problems)
                for n in numbers:
                    if str(n) not in legacy and n not in reserved:
                        errors.append(f"{path}:{number}: DECISIONS {n} names no entry")
            for issue, k in CITE_NEW.findall(line):
                if f"DEC-{issue}.{k}" not in where:
                    errors.append(f"{path}:{number}: DEC-{issue}.{k} names no entry")
    return errors


def main():
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    tracked = subprocess.check_output(["git", "ls-files", "-z"], cwd=root, text=True).split("\0")
    texts = {}
    for path in filter(None, tracked):
        try:
            texts[path] = (root / path).read_text(encoding="utf-8")
        except (UnicodeDecodeError, IsADirectoryError, FileNotFoundError):
            continue
    topics = {p: t for p, t in texts.items() if p.startswith(TOPICS) and p.endswith(".md")}
    if not topics:
        # An empty record is not a clean one: the walk found nothing to check.
        print(f"no topic files under {TOPICS}", file=sys.stderr)
        return 1
    errors = check(topics, texts[INDEX], texts)
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
