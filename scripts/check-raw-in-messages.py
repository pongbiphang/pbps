#!/usr/bin/env python3
"""Refuse `.raw()` inside a message a person reads (#2084, part C).

A name or a free text that can come from an artifact has no `Display`: it is
used as text only through `.shown()`, which escapes it, or `.raw()`, which does
not (`pbps_model::Text`, `TableName`/`ObjectName`, `ColumnRef`, `RoutineArg`,
`RoutineId`, `ModuleId`, `ColumnName`, `RoleName`, `ObjectLocalName`, `GrantTarget`,
`TypeRef`, `ColumnType` and `TypeArg`). The compiler forces that choice but cannot
stop `.raw()` being the one chosen for a message, and an unescaped line break in
a message starts a line the tool never wrote. This is the one hole the types
leave, so this check closes it.

It refuses `.raw()` (and the function path `Type::raw`) in the arguments of
`format!`, `bail!`, `anyhow!`, `ensure!`, `print!`, `println!`, `eprint!`,
`eprintln!`, `write!`, `writeln!` and in a `#[error(...)]` attribute, in any
Rust source under `crates/`, unless the file is in ALLOWED below. The macros
are the ones that build a message or an error for a person; `assert!` and
`panic!` report a bug in the tool, and almost all of theirs are tests. `.raw()` anywhere else -
a SQL bind, a map key, a comparison, a hash - is not this check's business.

SQL builders accept `format!` outside diagnostic sinks: `invalid`, `Err` and
`DbError`/`DialectError`/`RowsError` constructors. Their message macros and error
attributes are checked too. SQL-generated RAISE/THROW messages and shared
probe/offline descriptions are outside this bounded check (#2123); this does
not trace text through locals or unlisted helpers.

The other exemptions remain by path and, for known holes, count. Each entry
says why, and an entry whose file is gone, or whose count is no longer needed,
fails, so the list cannot go stale unread.
"""

import re
import subprocess
import sys
from pathlib import Path

SCANNED = "crates/"

MACROS = ("format", "bail", "anyhow", "ensure", "print", "println", "eprint", "eprintln", "write", "writeln")
# `\b` before the name: `my_format!` is another macro. The opening delimiter
# is the last character of the match.
MACRO_CALL = re.compile(r"\b(" + "|".join(MACROS) + r")\s*!\s*[(\[{]")
# thiserror's `#[error("..", args)]`; the opening parenthesis ends the match.
ERROR_ATTR = re.compile(r"#\s*\[\s*error\s*\(")
# A call, or the function itself passed along (`.map(Text::raw)`). Rust lets
# whitespace, a line break or a comment (blanked to whitespace by `mask`) sit
# between the pieces, and `r#raw` is the same name. `..raw` (struct update) is
# neither.
RAW = re.compile(r"\.\s*(?:r#)?raw\s*\(|::\s*(?:r#)?raw\b")
# The opening of a raw string, `r#"`; its prefix cannot follow an identifier.
RAW_STRING = re.compile(r"(?:br|cr|r)(#*)\"")
ERROR_CALL = re.compile(
    r"\b(?:\w+\s*::\s*)*(invalid|Err|(?:DbError|DialectError|RowsError)"
    r"\s*::\s*(?:r#)?[A-Z]\w*)\s*(?:::\s*<|[(\{])"
)
MESSAGE_FIELDS = {"message", "feature", "part", "detail", "why"}
SQL_BUILDERS = {
    f"crates/pbps-{engine}/src/{module}.rs"
    for engine in ("pg", "mssql")
    for module in ("emit", "preflight", "rows", "catalog")
} | {
    # Derives PostgreSQL index names for rebuilt partition trees.
    "crates/pbps-pg/src/partition_index/naming.rs",
    "crates/pbps-mssql/src/resolver/externals.rs",
    "crates/pbps-mssql/src/resolver/externals/render.rs",
    "crates/pbps-mssql/src/resolver/authorization/sandbox.rs",
    "crates/pbps-mssql/src/resolver/authorization/staged.rs",
    "crates/pbps-mssql/src/resolver/capture/object_properties.rs",
    "crates/pbps-mssql/src/resolver/capture/staged/names.rs",
}

# path -> (count, why). `count` is how many `.raw()` in a message the file may
# hold: None for a file that builds SQL or a document and so holds them by its
# nature, a number for a known hole, which has to equal what the file holds -
# a new one fails, and so does one fixed without lowering it.
ALLOWED = {
    "crates/pbps-model/src/types.rs": (
        4,
        "Three shown adapters escape canonical raw spellings; the canonical formatter preserves "
        "its argument bytes.",
    ),
    "crates/pbps-pg/src/type_refs.rs": (
        1,
        "A test constructs a cast expression as SQL.",
    ),
    "crates/pbps-cli/src/baseline.rs": (
        2,
        "role subjects are raw grouping keys, which remain raw",
    ),
    "crates/pbps-cli/src/declaration_file.rs": (
        1,
        "builds encoded role filenames from original bytes; the key remains raw",
    ),
    "crates/pbps-model/src/change.rs": (
        2,
        "role subjects are raw grouping and policy keys, which remain raw",
    ),
    "crates/pbps-cli/src/resolver/scope.rs": (
        1,
        "quotes a raw role into scratch SET ROLE SQL; SQL remains raw",
    ),
    "crates/pbps-mssql/src/resolver/authorization.rs": (
        22,
        "nineteen raw SQL operands and three raw test replay-row operands; both retain their "
        "original spelling",
    ),
    "crates/pbps-pg/src/resolver/authorization.rs": (
        5,
        "quotes raw role names into reconstruction SQL; SQL remains raw",
    ),
    "crates/pbps-pg/src/resolver/externals.rs": (
        1,
        "quotes a raw role into externals USAGE SQL; SQL remains raw",
    ),
    "crates/pbps-pg/src/resolver.rs": (
        1,
        "quotes raw role names into cleanup SQL; SQL remains raw",
    ),
    "crates/pbps-pg/src/emit.rs": (
        None,
        "builds PostgreSQL statements from raw names",
    ),
    "crates/pbps-pg/src/partition_index/naming.rs": (
        None,
        "derives PostgreSQL index names for rebuilt partition trees",
    ),
    "crates/pbps-pg/src/preflight.rs": (
        None,
        "builds PostgreSQL probe SQL from raw names",
    ),
    "crates/pbps-pg/src/rows.rs": (
        None,
        "builds PostgreSQL row reads and writes from raw names",
    ),
    "crates/pbps-pg/src/catalog.rs": (
        None,
        "the PostgreSQL catalog reader's SQL",
    ),
    "crates/pbps-mssql/src/emit.rs": (
        None,
        "builds SQL Server statements from raw names",
    ),
    "crates/pbps-mssql/src/preflight.rs": (
        None,
        "builds SQL Server probe SQL from raw names",
    ),
    "crates/pbps-mssql/src/rows.rs": (
        None,
        "builds SQL Server row reads and writes from raw names",
    ),
    "crates/pbps-mssql/src/catalog.rs": (
        None,
        "the SQL Server catalog reader's SQL",
    ),
    "crates/pbps-mssql/src/resolver/externals.rs": (
        None, "executes bounded externals batches and reads their complete catalog inventory",
    ),
    "crates/pbps-mssql/src/resolver/externals/render.rs": (
        None, "quotes catalog structure into SQL Server externals templates",
    ),
    "crates/pbps-mssql/src/resolver/authorization/sandbox.rs": (
        None,
        "quotes raw principal names into irreversible SQL Server deployer entry SQL",
    ),
    "crates/pbps-mssql/src/resolver/authorization/staged.rs": (
        None,
        "quotes raw owner, grantor and grantee names into SQL Server ownership and ACL replay SQL",
    ),
    "crates/pbps-mssql/src/resolver/capture/object_properties.rs": (
        None, "builds OBJECTPROPERTYEX queries from a fixed property manifest and catalog IDs",
    ),
    "crates/pbps-mssql/src/resolver/capture/staged/names.rs": (
        None, "quotes requested identity components into engine-resolved catalog name lookups",
    ),
    # Serialization: the bytes a file or a document holds, in that medium's own
    # escaping, never a message on the terminal.
    "crates/pbps-load/src/fmt.rs": (
        None,
        "writes the declaration YAML, quoting each value with its own `scalar`",
    ),
    "crates/pbps-load/src/managed_types.rs": (
        None,
        "writes the type declaration YAML, quoting each value with fmt's `scalar`",
    ),
    "crates/pbps-load/src/dependencies.rs": (
        None,
        "writes dependency declaration YAML, quoting each raw value with fmt's scalar",
    ),
    "crates/pbps-docs/src/html.rs": (
        None,
        "writes the HTML document, escaping each value with its own `esc`",
    ),
    "crates/pbps-docs/src/markdown.rs": (
        None,
        "writes the Markdown document, which is a file, not a terminal message",
    ),
    "crates/pbps-cli/src/report.rs": (
        9,
        "Canonical command arguments are passed to shell_arg, which quotes them or substitutes "
        "a non-executable placeholder.",
    ),
    "crates/pbps-model/src/name.rs": (
        2,
        "The canonical ColumnRef adapter writes raw TableName and ColumnName for serialization.",
    ),
    "crates/pbps-model/src/module.rs": (
        2,
        "Raw routine/trigger adapters preserve canonical serialization bytes.",
    ),
    "crates/pbps-pg/src/resolver/reconstruct.rs": (
        1,
        "builds a dropped routine signature as SQL; its names and arguments remain raw",
    ),
    "crates/pbps-diff/src/schema_diff.rs": (
        26,
        "One Rust Debug projection escapes a baseline-key list; twenty-five raw labels are test "
        "comparison data.",
    ),
    "crates/pbps-diff/src/identity.rs": (
        1,
        "matches a raw column target against its raw table-address prefix; this identity key "
        "remains raw",
    ),
    "crates/pbps-dialect/tests/support/positional_preflight.rs": (
        6,
        "builds fixture SQL, whose identifiers remain raw",
    ),
    "crates/pbps-pg/tests/live.rs": (
        5,
        "builds a routine's fixture SQL; SQL remains raw",
    ),
    "crates/pbps-cli/tests/flow_pg.rs": (
        1,
        "writes a trigger's fixture YAML; serialization remains raw",
    ),
    "crates/pbps-cli/src/dependents.rs": (
        6,
        "Frozen test ordering labels preserve raw role and local-name identities.",
    ),
    "crates/pbps-cli/src/dependents/split_oracle.rs": (
        1,
        "builds raw index-name suffix keys in the frozen split fixture; identity remains raw",
    ),
    "crates/pbps-cli/src/dependents/schedule.rs": (
        1,
        "a raw role-drop label is test comparison data, retained",
    ),
    "crates/pbps-mssql/tests/support/resolver.rs": (
        3,
        "two raw grant-row test labels and one raw DENY fixture SQL operand, all retained",
    ),
    "crates/pbps-pg/tests/support/resolver.rs": (
        11,
        "ten raw role fixture SQL operands and one raw role:extra comparison key, all retained",
    ),
    "crates/pbps-cli/src/status.rs": (
        2,
        "A raw SHA is shortened before Text::shown escapes it. The existing ledger reason can "
        "already be rendered failure prose; distinguishing legacy free text requires provenance "
        "and is retained.",
    ),
    "crates/pbps-cli/src/object_order.rs": (
        27,
        "raw index and constraint labels are test comparison data, retained",
    ),
    "crates/pbps-cli/tests/flow.rs": (
        1,
        "quotes a generated default-constraint name into fixture SQL; SQL remains raw",
    ),
    "crates/pbps-mssql/tests/live.rs": (
        2,
        "quotes constraint-name and overlength role-name targets into fixture SQL; SQL remains raw",
    ),
    "crates/pbps-pg/src/data_triggers.rs": (
        1,
        "quotes a raw foreign-key name into catalog probe SQL; SQL remains raw",
    ),
    "crates/pbps-docs/src/erd.rs": (
        2,
        "writes a Mermaid document label through its own quoted() escaping; document bytes "
        "remain raw",
    ),
    "crates/pbps-cli/src/engine.rs": (
        2,
        "Two operands build a removal note recorded in opaque StateSnapshot.reason and "
        "compared by its original truncation check. Stdout escapes the single data value.",
    ),
    "crates/pbps-policy/src/evaluate.rs": (
        6,
        "Five operands build raw declaration-finding comparison keys. The CLI compares these "
        "before projecting them to prose. One role subject is a canonical grouping/policy key.",
    ),
}


def mask(src):
    """`src` with comments, strings and character literals blanked.

    Newlines stay, so offsets and line numbers still point into `src`. What a
    string or a comment says is not code: `bail!("never .raw() here")` is not a
    hit, and a `)` in a string does not close the call.
    """
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
        elif c in "rbc" and not (i and (src[i - 1].isalnum() or src[i - 1] == "_")) and (
            opening := RAW_STRING.match(src, i)
        ):
            end = src.find('"' + opening[1], opening.end())
            j = n if end < 0 else end + 1 + len(opening[1])
            blank(opening.end() - 1, j)
            i = j
        elif c == '"':
            j = i + 1
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            blank(i, j + 1)
            i = j + 1
        elif c == "'":
            if src.startswith("\\", i + 1):
                j = src.find("'", i + 3)
                j = n if j < 0 else j + 1
            elif src.startswith("'", i + 2) and src[i + 1] != "'":
                j = i + 3
            else:
                # A lifetime: nothing to blank.
                j = i + 1
            blank(i, j)
            i = j
        else:
            i += 1
    return "".join(out)


def closing(code, opened):
    """The index of the delimiter closing the one at `opened`, or None.

    A closer that is not the opener's kind is not a close: `bail!("{}", f(x)\n}`
    is a call whose end is missing, not one ended by the next brace.
    """
    stack = []
    for i in range(opened, len(code)):
        if code[i] in "([{":
            stack.append(code[i])
        elif code[i] in ")]}":
            if not stack or "([{".index(stack.pop()) != ")]}".index(code[i]):
                return None
            if not stack:
                return i
    return None


def message_start(code, opened, end, name):
    """Where the message starts inside the call from `opened` to `end`.

    `ensure!(cond, "msg", args)` has a condition first, and comparing two names
    with `.raw()` there is right; only what follows its first top-level comma is
    a message. A comma inside a nested call or a generic list is not that comma
    (a generic's commas are read as top-level, which can only refuse more).
    """
    if name != "ensure!":
        return opened
    depth = 0
    for i in range(opened, end):
        if code[i] in "([{":
            depth += 1
        elif code[i] in ")]}":
            depth -= 1
        elif code[i] == "," and depth == 1:
            return i
    return end


def error_opening(code, match):
    """Find the call after optional type arguments, including nested types."""
    opened = match.end() - 1
    if code[opened] != "<":
        return opened
    depth, i = 1, opened + 1
    while i < len(code):
        c = code[i]
        if c in "([{":
            # Array lengths and const arguments can contain angle operators.
            end = closing(code, i)
            if end is None:
                return None
            i = end
        elif c == "<":
            depth += 1
        elif c == ">" and code[i - 1] != "-":  # not a function type's arrow
            depth -= 1
            if depth == 0:
                i += 1
                while i < len(code) and code[i].isspace():
                    i += 1
                return i if i < len(code) and code[i] in "({" else None
        elif c == ";":
            return None
        i += 1
    return None


def err_ranges(code, start, stop):
    """Read an Err's value through groups and the tail of a block.

    Local statements are not the returned prose. Their named diagnostic sinks
    are checked separately; tracing text through locals remains out of scope.
    """
    pending, ranges = [(start, stop)], []
    while pending:
        start, stop = pending.pop()
        while start < stop and code[start].isspace():
            start += 1
        if start == stop:
            continue
        if code[start] in "({":
            end = closing(code, start)
            if end is not None and end < stop:
                tail = start + 1
                if code[start] == "{":
                    i = tail
                    while i < end:
                        if code[i] in "([{":
                            i = closing(code, i)
                        elif code[i] == ";":
                            tail = i + 1
                        i += 1
                pending.extend([(tail, end), (end + 1, stop)])
                continue
        nested = ERROR_CALL.match(code, start)
        opened = error_opening(code, nested) if nested else None
        end = closing(code, opened) if opened is not None else None
        # The nested constructor checks its own fields, but a suffix can add
        # context whose raw operands are still part of this Err's message.
        ranges.append((end + 1 if end is not None else start, stop))
    return ranges


def error_ranges(code, opened, end, name):
    """The prose arguments or fields of a recognized error constructor.

    Typed fields (a table, a source error) and ByHand's statement are data,
    not prose. Typed wrappers delegate to their nested constructors so they
    cannot turn data fields back into messages. Err still checks any suffix
    after a nested constructor: a method there may add diagnostic context.
    """
    if code[opened] == "(":
        if re.sub(r"\s+|r#", "", name) == "RowsError::Dialect":
            return []
        if name == "Err":
            return err_ranges(code, opened + 1, end)
        return [(opened, end)]
    ranges, depth, start = [], 0, opened + 1
    for i in range(opened + 1, end + 1):
        c = code[i]
        # A generic argument's comma does not start the next struct field.
        next_field = c == "," and depth == 0 and re.match(
            r"\s*(?:(?:r#)?\w+\s*:(?!:)|\.\.)", code[i + 1:end]
        )
        if i == end or next_field:
            field = re.match(r"\s*(?:r#)?(\w+)\s*:(?!:)", code[start:i])
            if field and field[1] in MESSAGE_FIELDS:
                ranges.append((start + field.end(), i))
            start = i + 1
        elif c in "([{":
            depth += 1
        elif c in ")]}":
            depth -= 1
    return ranges


def hits(src, builder=False):
    """([(line, macro)] for each `.raw()` in a message, [line] for each call
    this could not find the end of)."""
    code = mask(src)
    line = lambda at: code.count("\n", 0, at) + 1
    calls = [(m.end() - 1, m[1] + "!", False) for m in MACRO_CALL.finditer(code)]
    calls += [(m.end() - 1, "#[error]", False) for m in ERROR_ATTR.finditer(code)]
    if builder:
        for m in ERROR_CALL.finditer(code):
            opened = error_opening(code, m)
            if opened is not None:
                calls.append((opened, m[1], True))
    found, lost = {}, []
    for opened, name, error in calls:
        end = closing(code, opened)
        if end is None:
            lost.append(line(opened))
            continue
        if builder and name == "format!":
            continue
        ranges = error_ranges(code, opened, end, name) if error else [
            (message_start(code, opened, end, name), end)
        ]
        for start, stop in ranges:
            for raw in RAW.finditer(code, start, stop):
                n = line(raw.start())
                found.setdefault(raw.start(), name)
    return [(line(at), name) for at, name in sorted(found.items())], lost


def check(files, allowed=ALLOWED):
    """files: {path: text} of every Rust source. Returns a list of errors."""
    errors = []
    unusable = set()
    for path, (count, why) in sorted(allowed.items()):
        if path not in files:
            errors.append(f"{path}: allowed, but there is no such file; remove the entry")
        if not why.strip():
            errors.append(f"{path}: allowed without saying why")
        if count is not None and (not isinstance(count, int) or count < 1):
            errors.append(f"{path}: the count is None or a positive number, not {count!r}")
            unusable.add(path)
    for path, text in sorted(files.items()):
        builder = path in SQL_BUILDERS
        found, lost = hits(text, builder=builder)
        # A call whose end is not found is a call whose arguments were not
        # read: that is not a clean file, whatever the file is for.
        errors.extend(f"{path}:{n}: cannot find the end of this call" for n in lost)
        if path in allowed and not builder:
            count = allowed[path][0]
            if count is not None and path not in unusable and len(found) != count:
                where = ", ".join(str(n) for n, _ in found) or "none"
                errors.append(
                    f"{path}: the allowlist expects {count} `.raw()` in a message and "
                    f"there are {len(found)} (lines: {where}); a new one is a message "
                    f"that prints a raw text, and a fixed one lowers the count"
                )
            continue
        for n, name in found:
            advice = (
                "SQL-builder exemptions cover formatting outside diagnostic sinks (#2123)"
                if builder else
                "If this file builds SQL or a document, add it to ALLOWED in "
                "scripts/check-raw-in-messages.py with the reason (#2084)"
            )
            errors.append(
                f"{path}:{n}: `.raw()` inside {name}: a message a person reads "
                f"prints it unescaped; use `.shown()`. {advice}"
            )
    return errors


def main():
    root = Path(subprocess.check_output(["git", "rev-parse", "--show-toplevel"], text=True).strip())
    tracked = subprocess.check_output(["git", "ls-files", "-z", "--", "*.rs"], cwd=root, text=True)
    files, errors = {}, []
    for path in filter(None, tracked.split("\0")):
        if not path.startswith(SCANNED):
            continue
        try:
            files[path] = (root / path).read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError) as e:
            # Unreadable is not clean.
            errors.append(f"{path}: cannot be read: {e}")
    if not files:
        # An empty walk is not a clean one: it found nothing to check.
        print(f"no Rust sources under {SCANNED}", file=sys.stderr)
        return 1
    errors += check(files)
    for error in errors:
        print(error, file=sys.stderr)
    return 1 if errors else 0


if __name__ == "__main__":
    sys.exit(main())
