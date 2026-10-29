"""Mechanical checks for the hand-maintained resolver acceptance table (#2487).

Source declarations and ownership are checked here. Neither semantic coverage nor
execution is inferred; the closing audit in #2489 reads actual job logs.
"""
from collections import defaultdict
from pathlib import Path
import json
import re
import sys

from ignored_test_inventory import rust_tokens

CASE_HEADER = ["Case", "Disposition", "Tests", "Reference", "Engine/build", "Scratch", "Platform"]
TEST_HEADER = ["ID", "Test", "Source", "Target", "Owner", "Job", "Platform",
               "Engine/build", "Scratch", "Evidence", "Delivery"]
DISPOSITIONS = {
    "tests", "superseded by DEC-952.1", "not required after DEC-952.1",
    "measured profiles only, retired by #1636", "operator-vouched", "not exercised",
}


class AcceptanceError(ValueError):
    pass


def require(ok, reason):
    if not ok:
        raise AcceptanceError(reason)


def table(document, marker, header):
    lines = document.splitlines()
    require(lines.count(marker) == 1, f"expected one {marker}")
    start = lines.index(marker) + 1
    while start < len(lines) and not lines[start].strip():
        start += 1
    rows = []
    for line in lines[start:]:
        if not line.startswith("|"):
            break
        require(line.endswith("|"), f"unterminated table row: {line}")
        cells = [c.strip().strip(chr(96)) for c in line[1:-1].split("|")]
        if all(re.fullmatch(r":?-+:?", c) for c in cells):
            continue
        require(len(cells) == len(header), f"wrong column count: {line}")
        rows.append(cells)
    require(rows and rows[0] == header, f"wrong header for {marker}")
    require(len(rows) > 1, f"empty {marker}")
    return [dict(zip(header, row)) for row in rows[1:]]


def source_tests(root, names):
    """Locate attributed definitions; ignore prose, SQL strings and helper fns."""
    definitions = defaultdict(list)
    candidate = re.compile(r"\bfn\s+(" + "|".join(map(re.escape, sorted(names))) + r")\b")
    for path in sorted((root / "crates").rglob("*.rs")):
        source = path.read_text(encoding="utf-8")
        if not candidate.search(source):
            continue
        tokens = rust_tokens(source)
        test, ignored, i = False, False, 0
        depth, modules, openings = 0, [], {}
        while i < len(tokens):
            token = tokens[i]
            if token == "#" and tokens[i + 1:i + 2] == ["["]:
                j, attribute_depth = i + 2, 1
                while j < len(tokens) and attribute_depth:
                    if tokens[j] == "[": attribute_depth += 1
                    elif tokens[j] == "]": attribute_depth -= 1
                    j += 1
                require(attribute_depth == 0, f"unterminated attribute in {path}")
                attr = tokens[i + 2:j - 1]
                test |= attr == ["test"] or attr[:4] == ["tokio", ":", ":", "test"]
                ignored |= attr[:1] == ["ignore"]
                i = j
                continue
            if token == "mod" and i + 2 < len(tokens) and tokens[i + 2] == "{":
                openings[i + 2] = tokens[i + 1]
            if token == "{":
                depth += 1
                if i in openings:
                    modules.append((depth, openings[i]))
            elif token == "}":
                if modules and modules[-1][0] == depth:
                    modules.pop()
                depth -= 1
            if token == "fn" and i + 1 < len(tokens):
                name = tokens[i + 1]
                if test and isinstance(name, str) and name in names:
                    definitions[name].append((path.relative_to(root).as_posix(), tuple(m[1] for m in modules), ignored))
                test, ignored = False, False
            elif isinstance(token, str) and token in {
                    "mod", "struct", "enum", "trait", "impl", "const", "static", "use", "type"}:
                test, ignored = False, False
            i += 1
    return definitions


def check(root):
    document = (root / "docs/RESOLVER-ACCEPTANCE.md").read_text(encoding="utf-8")
    inventory = json.loads((root / "scripts/ignored-test-owners.json").read_text(encoding="utf-8"))
    cases = table(document, "<!-- acceptance-cases -->", CASE_HEADER)
    tests = table(document, "<!-- acceptance-tests -->", TEST_HEADER)
    numbers, references = set(), set()
    for row in cases:
        require(row["Case"].isdigit(), f"invalid ADR case {row['Case']}")
        number = int(row["Case"])
        require(number not in numbers, f"duplicate ADR case {number}")
        numbers.add(number)
        disposition = row["Disposition"]
        require(disposition in DISPOSITIONS or re.fullmatch(r"limitation #[1-9][0-9]*", disposition),
                f"invalid disposition for case {number}: {disposition}")
        refs = {r.strip() for r in row["Tests"].split(",") if r.strip() and r.strip() != "-"}
        require(disposition != "tests" or refs, f"case {number} has no test")
        require(row["Reference"] not in {"", "-"}, f"case {number} has no reference")
        require(disposition != "operator-vouched" or "SPEC" in row["Reference"],
                f"operator-vouched case {number} needs a SPEC reference")
        for field in ("Engine/build", "Scratch", "Platform"):
            require(row[field] not in {"", "-"}, f"case {number}: missing {field}")
        references.update(refs)
    require(numbers == set(range(1, 25)), "ADR cases must be exactly 1 through 24")

    ids = [row["ID"] for row in tests]
    require(all(ids) and len(ids) == len(set(ids)), "duplicate or empty test reference")
    require(references <= set(ids), f"unknown test references: {sorted(references - set(ids))}")
    names = {row["Test"].split("::")[-1] for row in tests}
    adr = (root / "docs/ADR-0016-engine-assisted-planning.md").read_text(encoding="utf-8")
    marker = "**Acceptance tests for the resolver (live, PostgreSQL):**"
    require(adr.count(marker) == 1, "missing or ambiguous ADR amendment test list")
    section = adr.split(marker, 1)[1].split("## Architectural placement", 1)[0]
    amendment, continuation = set(), False
    for line in section.splitlines():
        line = line.strip()
        bullet = line.startswith("- ")
        if bullet or continuation:
            text = line[2:] if bullet else line
            match = re.match(chr(96) + r"([A-Za-z_][A-Za-z_0-9]*)" + chr(96), text)
            if match:
                amendment.add(match[1])
                amendment.update(re.findall(r"and " + chr(96) +
                                 r"([A-Za-z_][A-Za-z_0-9]*)" + chr(96), text[match.end():]))
            continuation = bool(match and text.endswith(" and"))
        else:
            continuation = False
    require(amendment, "empty ADR amendment test list")
    require(amendment <= names, f"amendment tests missing from table: {sorted(amendment - names)}")
    require(all(re.fullmatch(r"[A-Za-z_][A-Za-z_0-9]*", n) for n in names), "invalid test name")
    definitions = source_tests(root, names)
    for row in tests:
        label = row["ID"]
        case = tuple(row["Test"].split("::"))
        candidates = [(source, modules, ignored) for source, modules, ignored in definitions[case[-1]]
                      if source == row["Source"] and case[-len(modules) - 1:] == (*modules, case[-1])]
        # Out-of-line prefixes belong to Cargo's registered identity. Within
        # this source, the longest matching inline suffix selects the scope.
        longest = max((len(modules) for _, modules, _ in candidates), default=-1)
        matches = [(source, ignored) for source, modules, ignored in candidates if len(modules) == longest]
        require(len(matches) == 1,
                f"{label}: expected one test definition in source {row['Source']}, got {len(matches)}")
        source, ignored = matches[0]
        owner = inventory["owners"].get(row["Owner"])
        require(owner is not None, f"{label}: unknown owner {row['Owner']}")
        require(row["Job"] == owner.get("job"), f"{label}: job disagrees with owner")
        require(row["Platform"] == owner.get("platform"), f"{label}: platform disagrees with owner")
        if ignored:
            groups = [g for g in inventory["groups"]
                      if "/".join(g["target"]) == row["Target"] and row["Test"] in g.get("cases", [])]
            require(any(g["owner"] == row["Owner"] for g in groups),
                    f"{label}: ignored test has no matching target/owner registration")
        else:
            require(owner.get("ordinary") is True, f"{label}: ordinary test needs an ordinary owner")
        for field in ("Engine/build", "Scratch", "Evidence", "Delivery"):
            require(row[field] not in {"", "-"}, f"{label}: missing {field}")
    return len(cases), len(tests)


def main():
    root = Path(__file__).resolve().parents[1]
    try:
        cases, tests = check(root)
    except (AcceptanceError, OSError, ValueError, KeyError) as error:
        print(f"resolver acceptance: {error}", file=sys.stderr)
        return 1
    print(f"resolver acceptance: {cases} cases, {tests} named test mappings; execution is audited separately")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
