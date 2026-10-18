#!/usr/bin/env python3
"""Refuse `.raw()` inside a message a person reads (#2084, part C).

A name or a free text that can come from an artifact has no `Display`: it is
used as text only through `.shown()`, which escapes it, or `.raw()`, which does
not (`pbps_model::Text`, `TableName`/`ObjectName` and `ColumnRef` today; the
remaining name types follow in #2084). The compiler forces that choice but cannot
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

The allowlist is by path, because what makes a `.raw()` right is what the file
is for. Each entry says why, and an entry whose file is gone, or which a count
shows is no longer needed, fails, so the list cannot go stale unread.
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

# path -> (count, why). `count` is how many `.raw()` in a message the file may
# hold: None for a file that builds SQL or a document and so holds them by its
# nature, a number for a known hole, which has to equal what the file holds -
# a new one fails, and so does one fixed without lowering it.
ALLOWED = {
    # The SQL builders. A name or a text is quoted through the dialect into a
    # statement, a probe or a row bind, and a `format!` is how the statement
    # is put together. What these files print for a person goes through
    # `.shown()`, which the types make the only other choice.
    "crates/pbps-pg/src/emit.rs": (None, "builds PostgreSQL statements from raw names"),
    "crates/pbps-pg/src/preflight.rs": (None, "builds PostgreSQL probe SQL from raw names"),
    "crates/pbps-pg/src/rows.rs": (None, "builds PostgreSQL row reads and writes from raw names"),
    "crates/pbps-pg/src/catalog.rs": (None, "the PostgreSQL catalog reader's SQL"),
    "crates/pbps-mssql/src/emit.rs": (None, "builds SQL Server statements from raw names"),
    "crates/pbps-mssql/src/preflight.rs": (None, "builds SQL Server probe SQL from raw names"),
    "crates/pbps-mssql/src/rows.rs": (None, "builds SQL Server row reads and writes from raw names"),
    "crates/pbps-mssql/src/catalog.rs": (None, "the SQL Server catalog reader's SQL"),
    # Serialization: the bytes a file or a document holds, in that medium's own
    # escaping, never a message on the terminal.
    "crates/pbps-load/src/fmt.rs": (
        None,
        "writes the declaration YAML, quoting each value with its own `scalar`",
    ),
    "crates/pbps-docs/src/html.rs": (
        None,
        "writes the HTML document, escaping each value with its own `esc`",
    ),
    "crates/pbps-docs/src/markdown.rs": (
        None,
        "writes the Markdown document, which is a file, not a terminal message",
    ),
    # Counted exceptions: shared sentences await #2084 step 5. Stable raw keys,
    # adapters and fixture builders are counted too, so a new raw message in
    # those files cannot hide behind their non-message uses.
    "crates/pbps-cli/src/report.rs": (
        22,
        "Canonical shell arguments and describe() names stay raw: the latter is "
        "the JSON `risks[].changes` text too and the report escapes it whole; "
        "#2084 step 5 removes that cover",
    ),
    "crates/pbps-cli/src/explain/resolver.rs": (
        5,
        "surface() becomes shared JSON/Text and is escaped once when rendered; "
        "#2084 step 5 removes the shared-sentence boundary",
    ),
    "crates/pbps-dialect/src/lib.rs": (
        21,
        "Unchecked::for_change labels are shared JSON and explain prose, "
        "escaped at rendering until #2084 step 5",
    ),
    "crates/pbps-mssql/src/lib.rs": (
        3,
        "reserved-table refusals feed whole-escaped DialectError::Invalid "
        "until #2084 step 5",
    ),
    "crates/pbps-mssql/src/validate.rs": (
        13,
        "validation sentences feed whole-escaped DialectError fields, and the "
        "#1450 declaration note the whole-escaped `dialect.not-checked` note, "
        "until #2084 step 5",
    ),
    "crates/pbps-pg/src/lib.rs": (
        3,
        "reserved-schema/table refusals feed whole-escaped DialectError::Invalid "
        "until #2084 step 5",
    ),
    "crates/pbps-pg/src/validate.rs": (
        2,
        "target-kind refusals feed whole-escaped DialectError::Invalid "
        "until #2084 step 5",
    ),
    "crates/pbps-pg/src/partition_series.rs": (
        22,
        "series refusals and frontier descriptions feed whole-escaped "
        "DialectError::Invalid until #2084 step 5",
    ),
    "crates/pbps-policy/src/evaluate.rs": (
        6,
        "policy sentences are shared JSON and findings that report::changes "
        "escapes whole until #2084 step 5; subjects remain raw identity keys",
    ),
    "crates/pbps-model/src/name.rs": (
        1,
        "the raw ColumnRef adapter delegates to the raw TableName adapter; "
        "serialization remains raw after #2084 step 5",
    ),
    "crates/pbps-model/src/module.rs": (
        2,
        "raw routine/trigger serialization bridges until #2084 step 1b; "
        "#2084 step 5 must retain their canonical serialized spelling",
    ),
    "crates/pbps-diff/src/identity.rs": (
        1,
        "matches a raw column target against its raw table-address prefix; "
        "this identity key remains raw after #2084 step 5",
    ),
    "crates/pbps-diff/src/order/compare.rs": (
        5,
        "builds raw name-slot identity keys that remain raw after #2084 step 5",
    ),
    "crates/pbps-dialect/tests/support/positional_preflight.rs": (
        6,
        "builds fixture SQL, whose identifiers remain raw after #2084 step 5",
    ),
    "crates/pbps-pg/tests/live.rs": (
        1,
        "builds a routine's fixture SQL; SQL remains raw after #2084 step 5",
    ),
    "crates/pbps-cli/tests/flow_pg.rs": (
        1,
        "writes a trigger's fixture YAML; serialization remains raw after #2084 step 5",
    ),
    "crates/pbps-cli/src/adopt.rs": (
        3,
        'raw onboarding notices are escaped at their boundary until #2084 step 5 (DEC-2084.1)',
    ),
    "crates/pbps-cli/src/deploy.rs": (
        2,
        'data.adoption messages are shared finding JSON; text output escapes once until #2084 step 5',
    ),
    "crates/pbps-cli/src/engine.rs": (
        3,
        'removal descriptions are recorded state and logical-slot labels are connected-check JSON; text boundaries escape once until #2084 step 5',
    ),
    "crates/pbps-mssql/src/estimate.rs": (
        1,
        'Estimate.about feeds cost JSON; cost::render escapes the shared description until #2084 step 5',
    ),
    "crates/pbps-mssql/src/introspect.rs": (
        2,
        'raw onboarding notices are escaped at their boundary until #2084 step 5 (DEC-2084.1)',
    ),
    "crates/pbps-pg/src/estimate.rs": (
        29,
        'Estimate.about feeds cost JSON; cost::render escapes the shared description until #2084 step 5',
    ),
    "crates/pbps-pg/src/impact.rs": (
        4,
        'DialectError::Invalid escapes the complete refusal once until #2084 step 5',
    ),
    "crates/pbps-cli/src/status.rs": (
        2,
        "the failed-attempt `detail` prints the recorded reason raw, as the "
        "JSON `detail` does, until #2084 step 5 gives it a type of its own; "
        "and the 8-character sha prefix is cut raw and shown through "
        "`Text::shown`, so an escape is never cut in half",
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


def hits(src):
    """([(line, macro)] for each `.raw()` in a message, [line] for each call
    this could not find the end of)."""
    code = mask(src)
    line = lambda at: code.count("\n", 0, at) + 1
    calls = [(m.end() - 1, m[1] + "!") for m in MACRO_CALL.finditer(code)]
    calls += [(m.end() - 1, "#[error]") for m in ERROR_ATTR.finditer(code)]
    found, lost = {}, []
    for opened, name in calls:
        end = closing(code, opened)
        if end is None:
            lost.append(line(opened))
            continue
        for raw in RAW.finditer(code, message_start(code, opened, end, name), end):
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
        found, lost = hits(text)
        # A call whose end is not found is a call whose arguments were not
        # read: that is not a clean file, whatever the file is for.
        errors.extend(f"{path}:{n}: cannot find the end of this macro call" for n in lost)
        if path in allowed:
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
            errors.append(
                f"{path}:{n}: `.raw()` inside {name}: a message a person reads "
                f"prints it unescaped; use `.shown()`. If this file builds SQL or "
                f"a document, add it to ALLOWED in scripts/check-raw-in-messages.py "
                f"with the reason (#2084)"
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
