#!/usr/bin/env python3
"""Check that CI, live runners, ignored-test registries, and architecture docs
agree with `pbps_config::DialectName::ALL` (DEC-2798.18).

`crates/pbps-config/src/lib.rs` holds the single source of truth for selectable
engines (`DialectName`, `DialectName::ALL`, and `DialectName::as_str`). This
check reads that enum directly from its Rust source and verifies that every
engine either:
  - has server-engine CI entries (`ci.yml` live job + compatibility matrix cell,
    a live runner script, `scripts/live-shards.py` `RUNNERS` and `FIXTURES`,
    `scripts/compatibility-matrix.json`, `scripts/ignored-test-owners.json`,
    and `scripts/ignored-test-rules.json`), or
  - carries an explicit `in-process` marker with a reason in
    `scripts/engine-ci.json` (for engines such as SQLite that run without a
    container).

Every engine (server or in-process) must also appear in `docs/SPEC.md` §11.3
and `docs/ARCHITECTURE.md`'s architectural boundaries block.

Standard library only. Reads `.github/workflows/ci.yml`; never writes it.
"""

import ast
import json
import re
import sys
from pathlib import Path

# One reader of the workflow's job layout: a second copy would let the two
# checks disagree about what a job is.
from check_gate_needs import gate_needs as workflow_gate_needs
from check_gate_needs import jobs as workflow_jobs

CONFIG_SOURCE = "crates/pbps-config/src/lib.rs"
MARKER_FILE = "scripts/engine-ci.json"
WORKFLOW_FILE = ".github/workflows/ci.yml"
COMPAT_MATRIX_FILE = "scripts/compatibility-matrix.json"
SHARDS_FILE = "scripts/live-shards.py"
OWNERS_FILE = "scripts/ignored-test-owners.json"
RULES_FILE = "scripts/ignored-test-rules.json"
SPEC_FILE = "docs/SPEC.md"
ARCH_FILE = "docs/ARCHITECTURE.md"

SERVER_ONLY_KEYS = ("live_job", "live_runner", "shard_suite", "ignored_owner")
RAW_STRING = re.compile(r"(?:br|cr|r)(#*)\"")


def strip_rust_comments(src):
    """Return `src` with Rust line and block comments replaced by spaces,
    preserving string literals and line count."""
    out = list(src)
    n = len(src)

    def blank(start, end):
        for k in range(start, min(end, n)):
            if out[k] != "\n":
                out[k] = " "

    i = 0
    while i < n:
        c = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            blank(i, j)
            i = j
        elif src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif src.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            blank(i, j)
            i = j
        elif (
            c in "rbc"
            and not (i and (src[i - 1].isalnum() or src[i - 1] == "_"))
            and (opening := RAW_STRING.match(src, i))
        ):
            end = src.find('"' + opening[1], opening.end())
            i = n if end < 0 else end + 1 + len(opening[1])
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            i = j + 1
        elif c == "'":
            if src.startswith("\\", i + 1):
                j = src.find("'", i + 3)
                i = n if j < 0 else j + 1
            elif src.startswith("'", i + 2) and src[i + 1] != "'":
                i += 3
            else:
                i += 1
        else:
            i += 1
    return "".join(out)


def balanced_block(src, open_brace):
    """Return the substring inside `{ ... }` starting at `open_brace`, or None."""
    if open_brace < 0 or open_brace >= len(src) or src[open_brace] != "{":
        return None
    depth = 0
    in_string = False
    i = open_brace
    while i < len(src):
        c = src[i]
        if in_string:
            if c == "\\":
                i += 2
                continue
            if c == '"':
                in_string = False
        else:
            if c == '"':
                in_string = True
            elif c == "{":
                depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    return src[open_brace + 1 : i]
        i += 1
    return None


def parse_dialect_names(source):
    """Parse canonical wire names from `DialectName`, `DialectName::ALL`, and `as_str`.

    Returns `(ordered_wire_names, errors)`.
    """
    code = strip_rust_comments(source)
    errors = []

    enum_matches = list(re.finditer(r"\bpub\s+enum\s+DialectName\s*\{", code))
    if len(enum_matches) != 1:
        return [], [f"{CONFIG_SOURCE}: expected exactly one `pub enum DialectName`, found {len(enum_matches)}"]
    enum_body = balanced_block(code, enum_matches[0].end() - 1)
    if enum_body is None:
        return [], [f"{CONFIG_SOURCE}: unterminated `pub enum DialectName` block"]
    # Strip any attributes on enum variants before splitting on commas.
    cleaned_enum = re.sub(r"#\[[^\]]*\]", "", enum_body)
    variants = []
    for raw_item in cleaned_enum.split(","):
        item = raw_item.strip()
        if not item:
            continue
        if not re.fullmatch(r"[A-Za-z_][A-Za-z0-9_]*", item):
            errors.append(f"{CONFIG_SOURCE}: unrecognized DialectName variant `{item}`")
            continue
        variants.append(item)
    if not variants:
        return [], [f"{CONFIG_SOURCE}: `enum DialectName` defines no variants"]

    impl_matches = list(re.finditer(r"\bimpl\s+DialectName\s*\{", code))
    if len(impl_matches) != 1:
        return [], [f"{CONFIG_SOURCE}: expected exactly one `impl DialectName`, found {len(impl_matches)}"]
    impl_body = balanced_block(code, impl_matches[0].end() - 1)
    if impl_body is None:
        return [], [f"{CONFIG_SOURCE}: unterminated `impl DialectName` block"]

    all_match = re.search(
        r"\bpub\s+const\s+ALL\s*:\s*&\s*'static\s*\[\s*(?:Self|DialectName)\s*\]\s*=\s*&\s*\[([^\]]*)\]\s*;",
        impl_body,
    )
    if not all_match:
        return [], [f"{CONFIG_SOURCE}: missing `DialectName::ALL` constant"]
    all_variants = []
    for raw_item in all_match.group(1).split(","):
        item = raw_item.strip()
        if not item:
            continue
        m = re.fullmatch(r"(?:Self|DialectName)::([A-Za-z_][A-Za-z0-9_]*)", item)
        if not m:
            errors.append(f"{CONFIG_SOURCE}: unrecognized entry `{item}` in `DialectName::ALL`")
            continue
        all_variants.append(m.group(1))
    if len(all_variants) != len(set(all_variants)):
        errors.append(f"{CONFIG_SOURCE}: `DialectName::ALL` contains duplicate variants")
    for v in variants:
        if v not in all_variants:
            errors.append(f"{CONFIG_SOURCE}: variant `{v}` is missing from `DialectName::ALL`")
    for v in all_variants:
        if v not in variants:
            errors.append(f"{CONFIG_SOURCE}: `DialectName::ALL` names unknown variant `{v}`")

    fn_match = re.search(
        r"\bpub\s+(?:const\s+)?fn\s+as_str\s*\(\s*self\s*\)\s*->\s*&\s*'static\s+str\s*\{",
        impl_body,
    )
    if not fn_match:
        return [], [f"{CONFIG_SOURCE}: missing `DialectName::as_str` method"]
    fn_body = balanced_block(impl_body, fn_match.end() - 1)
    if fn_body is None:
        return [], [f"{CONFIG_SOURCE}: unterminated `DialectName::as_str` body"]

    wire_by_variant = {}
    for arm in re.finditer(
        r"(?:Self|DialectName)::([A-Za-z_][A-Za-z0-9_]*)\s*=>\s*\"([^\"]+)\"",
        fn_body,
    ):
        variant, wire = arm.group(1), arm.group(2)
        if variant in wire_by_variant:
            errors.append(f"{CONFIG_SOURCE}: duplicate `as_str` arm for `{variant}`")
        wire_by_variant[variant] = wire

    for v in variants:
        if v not in wire_by_variant:
            errors.append(f"{CONFIG_SOURCE}: `DialectName::as_str` has no arm for `{v}`")
    if len(set(wire_by_variant.values())) != len(wire_by_variant):
        errors.append(f"{CONFIG_SOURCE}: `DialectName::as_str` maps two variants to the same name")

    if errors:
        return [], errors
    ordered = [wire_by_variant[v] for v in all_variants]
    return ordered, []


def read_text(root, rel_path, errors):
    """Read a UTF-8 file relative to `root`; append an error on failure."""
    path = root / rel_path
    try:
        return path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError) as error:
        errors.append(f"{rel_path}: cannot be read: {error}")
        return None


def read_json(root, rel_path, errors):
    """Read and parse a JSON file relative to `root`; append an error on failure."""
    text = read_text(root, rel_path, errors)
    if text is None:
        return None
    try:
        return json.loads(text)
    except json.JSONDecodeError as error:
        errors.append(f"{rel_path}: invalid JSON: {error}")
        return None


def flow_list(lines, key_pattern):
    """Extract a single flow-style YAML list `[a, b, ...]` matching `key_pattern`."""
    for line in lines:
        m = re.fullmatch(key_pattern, line)
        if m:
            return [
                item.strip().strip("'\"")
                for item in m.group(1).split(",")
                if item.strip()
            ]
    return None


def parse_shards_tables(text):
    """Parse top-level `RUNNERS` and `FIXTURES` dicts from `scripts/live-shards.py`."""
    tree = ast.parse(text, filename=SHARDS_FILE)
    values = {}
    for node in tree.body:
        if isinstance(node, ast.Assign):
            for target in node.targets:
                if isinstance(target, ast.Name) and target.id in ("RUNNERS", "FIXTURES"):
                    values[target.id] = ast.literal_eval(node.value)
    return values.get("RUNNERS"), values.get("FIXTURES")


def parse_spec_dependency_labels(text):
    """Extract the first-column labels from `### 11.3 Principal dependencies` in SPEC.md."""
    lines = text.splitlines()
    marker = "### 11.3 Principal dependencies"
    try:
        idx = lines.index(marker)
    except ValueError:
        return None
    labels = []
    in_table = False
    for line in lines[idx + 1 :]:
        if line.startswith("|"):
            in_table = True
            cells = [c.strip() for c in line.strip().strip("|").split("|")]
            if cells and not all(re.fullmatch(r":?-+:?", c) for c in cells):
                labels.append(cells[0])
        elif in_table:
            break
    return labels[1:] if len(labels) > 1 else []


def parse_architecture_crates(text):
    """Extract crate names listed in the `## Architectural boundaries` code block."""
    lines = text.splitlines()
    marker = "## Architectural boundaries"
    try:
        idx = lines.index(marker)
    except ValueError:
        return None
    in_fence = False
    crates = set()
    for line in lines[idx + 1 :]:
        stripped = line.strip()
        if not in_fence:
            if stripped.startswith("```"):
                in_fence = True
            continue
        if stripped.startswith("```"):
            return crates
        m = re.match(r"^(pbps-[a-z0-9-]+)\b", line)
        if m:
            crates.add(m.group(1))
    return None


def check(root):
    """Run all engine-list consistency checks under `root`. Returns a list of errors."""
    root = Path(root)
    errors = []

    config_text = read_text(root, CONFIG_SOURCE, errors)
    if config_text is None:
        return errors
    engines, parse_errors = parse_dialect_names(config_text)
    if parse_errors:
        return errors + parse_errors

    marker = read_json(root, MARKER_FILE, errors)
    workflow_text = read_text(root, WORKFLOW_FILE, errors)
    compat = read_json(root, COMPAT_MATRIX_FILE, errors)
    shards_text = read_text(root, SHARDS_FILE, errors)
    owners_doc = read_json(root, OWNERS_FILE, errors)
    rules_doc = read_json(root, RULES_FILE, errors)
    spec_text = read_text(root, SPEC_FILE, errors)
    arch_text = read_text(root, ARCH_FILE, errors)

    if marker is None:
        return errors
    if not isinstance(marker, dict) or marker.get("version") != 1 or not isinstance(marker.get("engines"), dict):
        errors.append(f"{MARKER_FILE}: expected object with `version: 1` and `engines` mapping")
        return errors
    entries = marker["engines"]

    # Check marker coverage against `DialectName::ALL`.
    for engine in engines:
        if engine not in entries:
            errors.append(
                f"engine `{engine}`: {MARKER_FILE}: missing entry (declare mode `server` "
                f"with CI/live mappings or mode `in-process` with a reason)"
            )
    for engine in sorted(entries):
        if engine not in engines:
            errors.append(
                f"engine `{engine}`: {MARKER_FILE}: stale entry not listed in `DialectName` ({CONFIG_SOURCE})"
            )

    # Parse shared files once.
    jobs = None
    gate_needs = None
    compat_matrix_cells = None
    if workflow_text is not None:
        try:
            jobs = workflow_jobs(workflow_text)
        except ValueError as error:
            errors.append(f"{WORKFLOW_FILE}: {error}")
        else:
            if "gate" not in jobs:
                errors.append(f"{WORKFLOW_FILE}: missing `gate` job")
            else:
                gate_needs = workflow_gate_needs(jobs["gate"])
                if gate_needs is None:
                    errors.append(f"{WORKFLOW_FILE}: cannot parse `gate.needs` list")
            if "compatibility" not in jobs:
                errors.append(f"{WORKFLOW_FILE}: missing `compatibility` job")
            else:
                compat_matrix_cells = flow_list(jobs["compatibility"], r"\s+cell:\s*\[([^\]]*)\]\s*")
                if compat_matrix_cells is None:
                    errors.append(f"{WORKFLOW_FILE}: cannot parse `compatibility` matrix `cell` list")

    compat_by_engine = {}
    if compat is not None:
        if not isinstance(compat, dict) or not compat:
            errors.append(f"{COMPAT_MATRIX_FILE}: expected a non-empty JSON object of cells")
        else:
            for cell, row in compat.items():
                if not isinstance(row, dict) or not isinstance(row.get("engine"), str):
                    errors.append(f"{COMPAT_MATRIX_FILE}: cell `{cell}` is missing a string `engine` field")
                    continue
                eng = row["engine"]
                if eng not in engines:
                    errors.append(
                        f"engine `{eng}`: {COMPAT_MATRIX_FILE}: cell `{cell}` names an engine not in `DialectName`"
                    )
                compat_by_engine.setdefault(eng, []).append(cell)

    runners, fixtures = None, None
    if shards_text is not None:
        try:
            runners, fixtures = parse_shards_tables(shards_text)
        except (SyntaxError, ValueError) as error:
            errors.append(f"{SHARDS_FILE}: cannot parse `RUNNERS`/`FIXTURES`: {error}")
        else:
            if not isinstance(runners, dict) or not isinstance(fixtures, dict):
                errors.append(f"{SHARDS_FILE}: missing `RUNNERS` or `FIXTURES` dictionary")
                runners, fixtures = None, None

    owners = None
    if owners_doc is not None:
        if not isinstance(owners_doc, dict) or not isinstance(owners_doc.get("owners"), dict):
            errors.append(f"{OWNERS_FILE}: missing `owners` object")
        else:
            owners = owners_doc["owners"]

    rules = None
    if rules_doc is not None:
        if not isinstance(rules_doc, dict) or not isinstance(rules_doc.get("rules"), list):
            errors.append(f"{RULES_FILE}: missing `rules` list")
        else:
            rules = rules_doc["rules"]

    spec_labels = None
    if spec_text is not None:
        spec_labels = parse_spec_dependency_labels(spec_text)
        if spec_labels is None:
            errors.append(f"{SPEC_FILE}: missing `### 11.3 Principal dependencies` table")

    arch_crates = None
    if arch_text is not None:
        arch_crates = parse_architecture_crates(arch_text)
        if arch_crates is None:
            errors.append(f"{ARCH_FILE}: missing `## Architectural boundaries` code block")

    # Verify each engine's declared entries.
    for engine in engines:
        entry = entries.get(engine)
        if entry is None:
            continue
        if not isinstance(entry, dict):
            errors.append(f"engine `{engine}`: {MARKER_FILE}: entry must be a JSON object")
            continue

        mode = entry.get("mode")
        doc_label = entry.get("doc_label")
        crate = entry.get("crate")
        if not isinstance(doc_label, str) or not doc_label.strip():
            errors.append(f"engine `{engine}`: {MARKER_FILE}: missing non-empty `doc_label`")
        elif spec_labels is not None and doc_label not in spec_labels:
            errors.append(
                f"engine `{engine}`: {SPEC_FILE}: missing `{doc_label}` row in `### 11.3 Principal dependencies`"
            )

        if not isinstance(crate, str) or not crate.strip():
            errors.append(f"engine `{engine}`: {MARKER_FILE}: missing non-empty `crate`")
        elif arch_crates is not None and crate not in arch_crates:
            errors.append(
                f"engine `{engine}`: {ARCH_FILE}: missing `{crate}` in `## Architectural boundaries`"
            )

        if mode == "in-process":
            reason = entry.get("reason")
            if not isinstance(reason, str) or not reason.strip():
                errors.append(
                    f"engine `{engine}`: {MARKER_FILE}: `in-process` mode requires a non-empty `reason`"
                )
            for forbidden in SERVER_ONLY_KEYS:
                if forbidden in entry:
                    errors.append(
                        f"engine `{engine}`: {MARKER_FILE}: `in-process` mode must not set server key `{forbidden}`"
                    )
            if compat_by_engine.get(engine):
                errors.append(
                    f"engine `{engine}`: {COMPAT_MATRIX_FILE}: marked `in-process` in {MARKER_FILE}, "
                    f"but has compatibility cells {sorted(compat_by_engine[engine])}"
                )
            continue

        if mode != "server":
            errors.append(
                f"engine `{engine}`: {MARKER_FILE}: `mode` must be `server` or `in-process`, got {mode!r}"
            )
            continue

        missing_fields = [
            key for key in SERVER_ONLY_KEYS
            if not isinstance(entry.get(key), str) or not entry[key].strip()
        ]
        if missing_fields:
            errors.append(
                f"engine `{engine}`: {MARKER_FILE}: `server` mode missing fields: {', '.join(missing_fields)}"
            )
            continue

        live_job = entry["live_job"]
        live_runner = entry["live_runner"]
        shard_suite = entry["shard_suite"]
        ignored_owner = entry["ignored_owner"]

        # 1. ci.yml live job, gate.needs, and compatibility matrix
        if jobs is not None:
            if live_job not in jobs:
                errors.append(
                    f"engine `{engine}`: {WORKFLOW_FILE}: missing live job `{live_job}`"
                )
            if gate_needs is not None and live_job not in gate_needs:
                errors.append(
                    f"engine `{engine}`: {WORKFLOW_FILE}: `gate.needs` is missing live job `{live_job}`"
                )
            # A job that exists and gates the merge but no longer runs the
            # engine's ignored tests would pass every check above.
            if live_job in jobs and isinstance(crate, str) and not any(
                re.search(rf"\bcargo test\b.*-p {re.escape(crate)}\b.*--ignored\b", line)
                and not line.lstrip().startswith("#")
                for line in jobs[live_job]
            ):
                errors.append(
                    f"engine `{engine}`: {WORKFLOW_FILE}: live job `{live_job}` runs no "
                    f"`cargo test -p {crate} ... --ignored` step"
                )

        # 2. compatibility-matrix.json and ci.yml compatibility matrix
        engine_cells = compat_by_engine.get(engine, [])
        if compat is not None and not engine_cells:
            errors.append(
                f"engine `{engine}`: {COMPAT_MATRIX_FILE}: no compatibility cell with `\"engine\": \"{engine}\"`"
            )
        if compat_matrix_cells is not None:
            for cell in engine_cells:
                if cell not in compat_matrix_cells:
                    errors.append(
                        f"engine `{engine}`: {WORKFLOW_FILE}: `compatibility` matrix is missing cell `{cell}`"
                    )

        # 3. Live runner script
        runner_text = read_text(root, live_runner, errors)
        if runner_text is not None:
            expected_shard_call = f"scripts/live-shards.py shard {shard_suite}"
            if expected_shard_call not in runner_text:
                errors.append(
                    f"engine `{engine}`: {live_runner}: missing `{expected_shard_call}` invocation"
                )

        # 4. scripts/live-shards.py RUNNERS and FIXTURES
        if runners is not None and fixtures is not None:
            runner_name = Path(live_runner).name
            if shard_suite not in runners:
                errors.append(
                    f"engine `{engine}`: {SHARDS_FILE}: `RUNNERS` is missing suite `{shard_suite}`"
                )
            elif runners[shard_suite] != runner_name:
                errors.append(
                    f"engine `{engine}`: {SHARDS_FILE}: `RUNNERS[{shard_suite!r}]` is "
                    f"{runners[shard_suite]!r}, expected {runner_name!r}"
                )
            if shard_suite not in fixtures or not fixtures[shard_suite]:
                errors.append(
                    f"engine `{engine}`: {SHARDS_FILE}: `FIXTURES` is missing suite `{shard_suite}`"
                )

        # 5. scripts/ignored-test-owners.json and scripts/ignored-test-rules.json
        if owners is not None:
            if ignored_owner not in owners:
                errors.append(
                    f"engine `{engine}`: {OWNERS_FILE}: missing owner `{ignored_owner}`"
                )
            elif owners[ignored_owner].get("job") != live_job:
                errors.append(
                    f"engine `{engine}`: {OWNERS_FILE}: owner `{ignored_owner}` targets job "
                    f"{owners[ignored_owner].get('job')!r}, expected `{live_job}`"
                )

        if rules is not None:
            if not any(isinstance(r, dict) and r.get("owner") == ignored_owner for r in rules):
                errors.append(
                    f"engine `{engine}`: {RULES_FILE}: no rule with `\"owner\": \"{ignored_owner}\"`"
                )

    return errors


def main(argv=None):
    root = Path(argv[0]) if argv else Path(__file__).resolve().parents[1]
    errors = check(root)
    for error in errors:
        print(error, file=sys.stderr)
    if errors:
        return 1
    print("engine lists consistent across DialectName, CI, live runners, registries, and docs")
    return 0


if __name__ == "__main__":
    sys.exit(main())
