#!/usr/bin/env python3
"""Move one family of `Change` kinds under its own enum (#2790).

A one-off tool for the step of #2790 that turns `Change` into an enum of
families. It is removed, with its test and its line in ci.yml, when that
step lands.

Usage:
    change_family_rewrite.py FAMILY OUTER INNER ROOT [--check] [--fail-on-report]
    change_family_rewrite.py FAMILY OUTER INNER ROOT --move-enum

    change_family_rewrite.py extensions Extension ExtensionChange . --check

`--move-enum` is the step before the rewrite: it moves the variants that
`change_family!(FAMILY)` names, with their docs and attributes, out of
`enum Change` into a new `enum INNER`, adds `#[serde(untagged)] OUTER(INNER)`
to `Change` and the re-export in lib.rs. It does not touch a consumer.

`Change` is becoming an enum of families, each family an enum of its kinds:
`Change::CreateExtension { .. }` turns into
`Change::Extension(ExtensionChange::CreateExtension { .. })`. This script does
that rewrite for one family. About ten thousand mentions are involved, so it is
done by a tool that understands enough Rust to be right and says so when it is
not sure. Standard library only.

What it rewrites (every path that names one of the family's kinds as a variant
of the model's `Change`, in patterns and expressions alike, with its payload):

    Change::K { .. }                  -> Change::F(FChange::K { .. })
    Change::K(x)                      -> Change::F(FChange::K(x))
    pbps_model::Change::K { .. }      -> pbps_model::Change::F(pbps_model::FChange::K { .. })
    $crate::Change::K { .. }          -> $crate::Change::F($crate::FChange::K { .. })
    C::K { .. }   (C = Change renamed) -> C::F(FChange::K { .. })

A qualifier is kept and given to the inner enum. A bare use needs `FChange` in
scope, so the script adds `use <same path>::FChange;` after the import that
brings `Change` into scope (and after the import that brought it to that
import, when it came through `super::`).

What it does NOT decide, and lists with file:line and a reason:
`Self::K` inside `impl Change`; `use Change::*` / `use Change::{K}` and a bare
`K` that follows them; `Change::$x`; occurrences inside `macro_rules!` bodies
(other than `$crate::`-qualified ones) and inside invocations of macros it does
not know to be transparent; a path split across lines or by a comment; a kind
used as a function (`.map(Change::K)`); a `Change` it cannot prove is the
model's; doc comments and string literals that name `Change::K`.

The macros `matches!`, `assert!` and the other std macros take Rust syntax as
their arguments, so their argument lists are rewritten like any other code.

Not touched, by construction: another enum with a variant of the same name
(`Intent::DropTable`), `ChangeFamily::...`, comments, strings, and a kind that
is not in the family. A second run changes nothing.
"""

import argparse
import bisect
import re
import sys
from collections import Counter
from pathlib import Path
from typing import NamedTuple

MODEL_CRATE_DIR = "crates/pbps-model"
MODEL_CRATE_NAME = "pbps_model"
CHANGE_FILE = "crates/pbps-model/src/change.rs"

# Std macros whose arguments are ordinary Rust expressions and patterns.
TRANSPARENT_MACROS = frozenset(
    "matches assert assert_eq assert_ne debug_assert debug_assert_eq "
    "debug_assert_ne vec format print println eprint eprintln write writeln "
    "panic unreachable todo unimplemented dbg assert_matches".split()
)

SKIP_DIRS = {"target", ".git", "node_modules", ".venv"}


class Tok(NamedTuple):
    kind: str  # ident, punct, str, char, life, num
    text: str
    start: int
    end: int


class Comment(NamedTuple):
    start: int
    end: int
    doc: bool


IDENT_RE = re.compile(r"[^\W\d]\w*")
RAW_STR_RE = re.compile(r'(?:br|cr|r)(#*)"')
NUM_RE = re.compile(r"\d[0-9A-Za-z_]*(?:\.\d[0-9A-Za-z_]*)?")


class LexError(Exception):
    pass


def lex(src):
    """Tokens and comments of Rust source. Strings, chars and comments are
    single tokens or comments, so nothing inside them is ever a path."""
    toks, comments = [], []
    i, n = 0, len(src)
    while i < n:
        c = src[i]
        if c.isspace():
            i += 1
            continue
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j < 0 else j
            text = src[i:j]
            doc = (text.startswith("///") and not text.startswith("////")) or text.startswith("//!")
            comments.append(Comment(i, j, doc))
            i = j
            continue
        if src.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                else:
                    j += 1
            if depth:
                raise LexError("unterminated block comment")
            text = src[i:j]
            doc = (text.startswith("/**") and not text.startswith("/***") and text != "/**/") or text.startswith("/*!")
            comments.append(Comment(i, j, doc))
            i = j
            continue
        m = RAW_STR_RE.match(src, i)
        if m:
            close = '"' + m.group(1)
            j = src.find(close, m.end())
            if j < 0:
                raise LexError("unterminated raw string")
            j += len(close)
            toks.append(Tok("str", src[i:j], i, j))
            i = j
            continue
        if c == '"' or (c in "bc" and src.startswith('"', i + 1)):
            j = i + (1 if c == '"' else 2)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            if j >= n:
                raise LexError("unterminated string")
            j += 1
            toks.append(Tok("str", src[i:j], i, j))
            i = j
            continue
        if c == "b" and src.startswith("b'", i):
            j = src.find("'", i + 3) if src[i + 2] == "\\" else i + 3
            if j < 0:
                raise LexError("unterminated byte char")
            toks.append(Tok("char", src[i : j + 1], i, j + 1))
            i = j + 1
            continue
        if c == "'":
            if i + 1 < n and src[i + 1] == "\\":
                j = src.find("'", i + 3)
                if j < 0:
                    raise LexError("unterminated char")
                toks.append(Tok("char", src[i : j + 1], i, j + 1))
                i = j + 1
            elif i + 2 < n and src[i + 2] == "'":
                toks.append(Tok("char", src[i : i + 3], i, i + 3))
                i += 3
            else:
                m = IDENT_RE.match(src, i + 1)
                j = m.end() if m else i + 1
                toks.append(Tok("life", src[i:j], i, j))
                i = j
            continue
        if c == "$":
            m = IDENT_RE.match(src, i + 1)
            if m:
                toks.append(Tok("ident", "$" + m.group(), i, m.end()))
                i = m.end()
                continue
        m = IDENT_RE.match(src, i)
        if m:
            j = m.end()
            if m.group() == "r" and src.startswith("#", j):
                m2 = IDENT_RE.match(src, j + 1)
                if m2:
                    j = m2.end()
            toks.append(Tok("ident", src[i:j], i, j))
            i = j
            continue
        m = NUM_RE.match(src, i)
        if m:
            toks.append(Tok("num", m.group(), i, m.end()))
            i = m.end()
            continue
        two = src[i : i + 2]
        if two in ("::", "=>", "->"):
            toks.append(Tok("punct", two, i, i + 2))
            i += 2
            continue
        toks.append(Tok("punct", c, i, i + 1))
        i += 1
    return toks, comments


OPENERS = {"(": ")", "[": "]", "{": "}"}


def match_brackets(toks):
    """Index of the closing token of each opening bracket, and the reverse."""
    close_of, stack = {}, []
    for i, t in enumerate(toks):
        if t.kind != "punct":
            continue
        if t.text in OPENERS:
            stack.append(i)
        elif t.text in ")]}":
            if not stack or OPENERS[toks[stack[-1]].text] != t.text:
                raise LexError("unbalanced brackets")
            close_of[stack.pop()] = i
    if stack:
        raise LexError("unbalanced brackets")
    return close_of


class Entry(NamedTuple):
    segs: tuple  # path segments; `self` already folded away
    alias: object  # the `as` name or None
    glob: bool
    leading: bool  # the path began with `::`


class UseStmt(NamedTuple):
    entries: list
    start: int  # offset of the first attribute, `pub` or `use`
    use_start: int  # offset of the visibility or `use` keyword
    end: int  # offset just after `;`
    vis: str  # `pub `, `pub(crate) ` or ``
    scope: int
    line_start: int


class Scope:
    """A brace group. `use` items and declarations belong to the innermost one
    (a module body, a function body, any block), and a name is looked up from
    there outwards until the enclosing module."""

    def __init__(self, parent, is_mod=False):
        self.parent = parent  # index of the enclosing scope in this file, or None
        self.is_mod = is_mod
        self.uses = []
        self.decls = set()  # names of enum/struct/type/trait declared here
        self.change_aliases = set()  # of those, the `type X = ...Change;` ones


class Facts:
    """What one file says, found in one pass over its tokens."""

    def __init__(self, path, src, toks, comments):
        self.path, self.src, self.toks, self.comments = path, src, toks, comments
        self.close_of = match_brackets(toks)
        self.line_starts = [0] + [m.end() for m in re.finditer("\n", src)]
        self.scopes = [Scope(None, True)]
        self.scope_of = [0] * len(toks)  # scope index per token
        self.ctx_of = [None] * len(toks)  # innermost macro context per token
        self.impl_change = [False] * len(toks)
        self.type_alias = {}  # name -> True when `type name = ...Change;` anywhere (a filter only)
        self.user_macros = set()  # macros this file defines, which shadow a std one of the name
        self.uses = []
        self._walk()

    def line(self, offset):
        return bisect.bisect_right(self.line_starts, offset)

    def _walk(self):
        toks, close_of = self.toks, self.close_of
        scope_stack = [(len(toks), 0)]  # (close index, scope index)
        ctx_stack = [(len(toks), None)]
        impl_stack = [(len(toks), False)]
        i = 0
        while i < len(toks):
            while len(scope_stack) > 1 and i > scope_stack[-1][0]:
                scope_stack.pop()
            while len(ctx_stack) > 1 and i > ctx_stack[-1][0]:
                ctx_stack.pop()
            while len(impl_stack) > 1 and i > impl_stack[-1][0]:
                impl_stack.pop()
            t = toks[i]
            cur = scope_stack[-1][1]
            self.scope_of[i] = cur
            self.ctx_of[i] = ctx_stack[-1][1]
            self.impl_change[i] = impl_stack[-1][1]
            if t.kind == "punct" and t.text in OPENERS and i in close_of:
                ctx = self._group_context(i)
                if ctx is not None:
                    ctx_stack.append((close_of[i], ctx))
                    # the opening bracket itself is outside; its contents inside
            if t.text == "{" and i in close_of:
                idx = len(self.scopes)
                is_mod = i >= 2 and toks[i - 2].text == "mod" and toks[i - 1].kind == "ident"
                self.scopes.append(Scope(cur, is_mod))
                scope_stack.append((close_of[i], idx))
            if t.kind == "ident":
                if t.text in ("enum", "struct", "trait", "union") and i + 1 < len(toks) and toks[i + 1].kind == "ident":
                    self.scopes[cur].decls.add(toks[i + 1].text)
                elif t.text == "type" and i + 1 < len(toks) and toks[i + 1].kind == "ident":
                    name = toks[i + 1].text
                    self.scopes[cur].decls.add(name)
                    j = i + 2
                    while j < len(toks) and toks[j].text != ";":
                        j += 1
                    if j > i + 2 and toks[j - 1].text == "Change" and toks[j - 1].kind == "ident":
                        self.type_alias[name] = True
                        self.scopes[cur].change_aliases.add(name)
                elif t.text == "macro_rules" and i + 2 < len(toks) and toks[i + 1].text == "!":
                    self.user_macros.add(toks[i + 2].text)
                elif t.text == "use" and not (i > 0 and toks[i - 1].text in ("::", ".")):
                    self._use(i, cur)
                elif t.text == "impl":
                    self._impl(i, impl_stack)
            i += 1

    def _group_context(self, i):
        toks = self.toks
        if toks[i].text == "[" and i > 0 and toks[i - 1].text in ("#", "!"):
            return None
        if i >= 2 and toks[i - 1].kind == "ident" and toks[i - 2].text == "!":
            if i >= 3 and toks[i - 3].text == "macro_rules":
                return ("macro_rules", toks[i - 1].text)
            return None
        if i >= 2 and toks[i - 1].text == "!" and toks[i - 2].kind == "ident":
            name = toks[i - 2].text
            if name == "macro_rules":
                return None
            return ("macro", name)
        return None

    def _impl(self, i, impl_stack):
        toks = self.toks
        j = i + 1
        while j < len(toks) and toks[j].text not in ("{", ";"):
            if toks[j].text in OPENERS and j in self.close_of and toks[j].text != "{":
                j = self.close_of[j]
            j += 1
        if j >= len(toks) or toks[j].text != "{" or j not in self.close_of:
            return
        header = [t for t in toks[i + 1 : j]]
        texts = [t.text for t in header]
        if "where" in texts:
            header = header[: texts.index("where")]
            texts = texts[: len(header)]
        if "for" in texts:
            header = header[len(texts) - texts[::-1].index("for") :]
            texts = [t.text for t in header]
        elif texts and texts[0] == "<":  # skip `impl<T>`
            depth = 0
            for k, x in enumerate(texts):
                depth += x == "<"
                depth -= x == ">"
                if depth == 0:
                    header, texts = header[k + 1 :], texts[k + 1 :]
                    break
        names_only = all(t.kind == "ident" or t.text == "::" for t in header)
        is_change = bool(header) and names_only and header[-1].text == "Change"
        impl_stack.append((self.close_of[j], is_change))

    def _use(self, i, scope):
        toks = self.toks
        j = i + 1
        while j < len(toks) and toks[j].text != ";":
            if toks[j].text in OPENERS and j in self.close_of:
                j = self.close_of[j]
            j += 1
        if j >= len(toks):
            return
        entries = []
        parse_use_tree(toks, i + 1, j, self.close_of, [], entries)
        # visibility and attributes before `use`
        k = i
        vis_start = i
        if k >= 1 and toks[k - 1].text == ")":
            open_i = next((o for o, c in self.close_of.items() if c == k - 1), None)
            if open_i is not None and open_i >= 1 and toks[open_i - 1].text == "pub":
                vis_start = open_i - 1
        elif k >= 1 and toks[k - 1].text == "pub":
            vis_start = k - 1
        attr_start = vis_start
        while attr_start >= 1 and toks[attr_start - 1].text == "]":
            open_i = next((o for o, c in self.close_of.items() if c == attr_start - 1), None)
            if open_i is not None and open_i >= 1 and toks[open_i - 1].text == "#":
                attr_start = open_i - 1
            else:
                break
        vis = self.src[toks[vis_start].start : toks[i].start]
        stmt = UseStmt(
            entries,
            toks[attr_start].start,
            toks[vis_start].start,
            toks[j].end,
            vis,
            scope,
            self.line_starts[self.line(toks[attr_start].start) - 1],
        )
        self.scopes[scope].uses.append(stmt)
        self.uses.append(stmt)


def parse_use_tree(toks, j, end, close_of, prefix, entries):
    """Flatten the use tree starting at `j` into entries; returns the index
    after the subtree."""
    segs = list(prefix)
    leading = False
    while j < end:
        t = toks[j]
        if t.text == "::":
            if not segs:
                leading = True
            j += 1
            continue
        if t.text == "{":
            close = close_of[j]
            k = j + 1
            while k < close:
                k = parse_use_tree(toks, k, close, close_of, segs, entries)
                if k < close and toks[k].text == ",":
                    k += 1
            return close + 1
        if t.text == "*":
            entries.append(Entry(tuple(segs), None, True, leading))
            return j + 1
        if t.kind == "ident":
            segs.append(t.text)
            j += 1
            if j < end and toks[j].text == "::":
                j += 1
                continue
            alias = None
            if j < end and toks[j].text == "as" and j + 1 < len(toks):
                alias = toks[j + 1].text
                j += 2
            final = tuple(segs)
            if len(final) > 1 and final[-1] == "self":
                final = final[:-1]
            entries.append(Entry(final, alias, False, leading))
            return j
        break
    return j


def read_family(root, family, outer, inner):
    """The kinds of one family and the shape of each, from change.rs itself."""
    path = root / CHANGE_FILE
    src = path.read_text()
    toks, _ = lex(src)
    close_of = match_brackets(toks)
    kinds = None
    for i, t in enumerate(toks):
        if t.text == "macro_rules" and i + 3 < len(toks) and toks[i + 2].text == "change_family":
            body_open = i + 3
            end = close_of[body_open]
            k = body_open + 1
            while k < end:
                if (
                    toks[k].text == "("
                    and toks[k + 1].text == family
                    and toks[k + 2].text == ")"
                    and toks[k + 3].text == "=>"
                ):
                    arm = k + 4
                    arm_end = close_of[arm]
                    kinds = []
                    for m in range(arm + 1, arm_end - 1):
                        if toks[m + 1].text == "::" and toks[m + 2].kind == "ident":
                            if toks[m].text == "Change" and toks[m + 2].text != outer:
                                kinds.append(toks[m + 2].text)
                            elif toks[m].text == inner:
                                kinds.append(toks[m + 2].text)
                    break
                k += 1
    if not kinds:
        raise SystemExit(f"family `{family}` not found in change_family! in {CHANGE_FILE}")
    shapes = {}
    for enum_name in ("Change", inner):
        for i, t in enumerate(toks):
            if t.text == "enum" and i + 2 < len(toks) and toks[i + 1].text == enum_name and toks[i + 2].text == "{":
                k, end = i + 3, close_of[i + 2]
                while k < end:
                    while toks[k].text == "#":  # attribute
                        k = close_of[k + 1] + 1
                    name = toks[k]
                    nxt = toks[k + 1].text
                    shape = {"{": "struct", "(": "tuple"}.get(nxt, "unit")
                    if name.kind == "ident":
                        shapes.setdefault(name.text, shape)
                    k += 1
                    if shape != "unit":
                        k = close_of[k] + 1
                    while k < end and toks[k].text != ",":
                        k += 1
                    k += 1
    return list(dict.fromkeys(kinds)), shapes


class Edit(NamedTuple):
    pos: int
    order: int  # at one position, closings (0) go before openings (1)
    text: str


class Project:
    def __init__(self, root, family, outer, inner, kinds, shapes):
        self.root = root
        self.family, self.outer, self.inner = family, outer, inner
        self.kinds = set(kinds)
        self.shapes = shapes
        self._facts = {}
        self.reports = []  # (rel, line, reason, text)
        self.notes = []
        self.rewrites = Counter()  # rel -> number of sites
        self.import_edits = {}  # (path, stmt start) -> text
        self.edits = {}  # path -> [Edit]
        self.comment_mentions = 0
        self.skipped_other = Counter()

    # ---- files and scopes -------------------------------------------------
    def rel(self, path):
        return str(path.relative_to(self.root))

    def facts(self, path):
        if path not in self._facts:
            src = path.read_text()
            try:
                toks, comments = lex(src)
                self._facts[path] = Facts(path, src, toks, comments)
            except LexError as e:
                self._facts[path] = e
        return self._facts[path]

    def crate_info(self, path):
        """(crate directory, in the model crate, kind of dir: src/tests/...)."""
        parts = path.relative_to(self.root).parts
        if len(parts) >= 3 and parts[0] == "crates":
            return Path(*parts[:2]), parts[1] == "pbps-model", parts[2]
        return None, False, None

    def crate_root_file(self, path):
        crate_dir, _, kind = self.crate_info(path)
        if crate_dir is None:
            return None
        if kind == "src":
            for name in ("lib.rs", "main.rs"):
                p = self.root / crate_dir / "src" / name
                if p.exists():
                    return p
            return None
        # tests/x.rs and tests/x/y.rs belong to the crate rooted at tests/x.rs
        parts = path.relative_to(self.root).parts
        top = self.root.joinpath(*parts[:4])
        return top if top.suffix == ".rs" else top.with_suffix(".rs")

    def declarations(self):
        """File -> (declaring file, scope index there, 'mod' | 'include'), for
        modules written `#[path = "x.rs"] mod m;` and files `include!("x.rs")`."""
        if hasattr(self, "_declarations"):
            return self._declarations
        found = {}
        for path in rust_files(self.root):
            text = path.read_text()
            if "#[path" not in text and "include!(" not in text:
                continue
            f = self.facts(path)
            if isinstance(f, LexError):
                continue
            toks = f.toks
            for i, t in enumerate(toks):
                mode = target = None
                if t.text == "#" and i + 5 < len(toks) and toks[i + 1].text == "[" and toks[i + 2].text == "path" and toks[i + 3].text == "=" and toks[i + 4].kind == "str":
                    mode, target = "mod", toks[i + 4].text
                elif t.text == "include" and i + 3 < len(toks) and toks[i + 1].text == "!" and toks[i + 2].text == "(" and toks[i + 3].kind == "str" and toks[i + 4].text == ")":
                    mode, target = "include", toks[i + 3].text
                if target is None or not target.startswith('"'):
                    continue
                literal = target.strip('"')
                for base in (path.parent, path.parent / path.stem):
                    cand = (base / literal).resolve()
                    if cand.exists() and cand not in found:
                        found[cand] = (path, f.scope_of[i], mode)
        self._declarations = found
        return found

    def parent_module_file(self, path):
        """(file, scope index) of the module that declares this file's module."""
        crate_dir, _, kind = self.crate_info(path)
        if crate_dir is None or path.name in ("lib.rs", "main.rs"):
            return None
        decl = self.declarations().get(path.resolve())
        if decl is not None:
            return decl
        container = path.parent.parent if path.name == "mod.rs" else path.parent
        modname = path.parent.name if path.name == "mod.rs" else path.stem
        for cand in (
            container / "mod.rs",
            container.parent / (container.name + ".rs"),
            container / "lib.rs",
            container / "main.rs",
        ):
            if cand.exists() and cand != path:
                f = self.facts(cand)
                if isinstance(f, LexError):
                    continue
                for i, t in enumerate(f.toks):
                    if t.text == "mod" and i + 1 < len(f.toks) and f.toks[i + 1].text == modname:
                        return cand, f.scope_of[i], "mod"
        return None

    def module_scope(self, f, scope):
        """The module scope that encloses a block scope."""
        while not f.scopes[scope].is_mod:
            scope = f.scopes[scope].parent
        return scope

    def super_of(self, path, scope):
        """The (file, scope) that `super` names from a scope."""
        f = self.facts(path)
        scope = self.module_scope(f, scope)
        parent = f.scopes[scope].parent
        if parent is not None:
            return path, self.module_scope(f, parent)
        decl = self.parent_module_file(path)
        if decl is None:
            return None
        dpath, dscope, mode = decl
        return self.super_of(dpath, dscope) if mode == "include" else (dpath, dscope)

    # ---- what does `Change` / `X` mean in a scope -------------------------
    def resolve(self, path, scope, name, depth=0):
        """What `name` is where a site stands, as (kind, anchor, prefix).

        kind: 'model' (the model's `Change`, brought in by an import or a
        glob), 'model-own' (declared here), 'model-glob' (`pbps_model::*`
        is in scope), 'other' (some other type) or 'unknown'.
        anchor: the `use` statement in the site's own scope chain that brings
        it in, (path, UseStmt), where the import of the inner enum goes.
        prefix: the path that statement's target is written with, so the
        inner enum is named the same way (`pbps_model`, `crate`, `super`)."""
        none = None
        if depth > 8:
            return ("unknown", none, none)
        f = self.facts(path)
        if isinstance(f, LexError):
            return ("unknown", none, none)
        _, in_model, _ = self.crate_info(path)
        s = scope
        while True:
            sc = f.scopes[s]
            for stmt in sc.uses:
                for e in stmt.entries:
                    if e.glob or (e.alias or e.segs[-1]) != name:
                        continue
                    if e.segs[-1] != "Change":
                        return ("other", none, none)
                    s0 = e.segs[0]
                    absolute = "::".join(e.segs[:-1])
                    if e.leading:
                        absolute = "::" + absolute
                    if s0 == MODEL_CRATE_NAME or self.alias_status(path, s, s0) == "model" or (s0 == "crate" and in_model):
                        return ("model", (path, stmt), absolute)
                    if s0 in ("crate", "super", "self"):
                        target = self.relative_target(path, s, e.segs)
                        if target is not None:
                            kind, _, prefix = self.resolve(target[0], target[1], "Change", depth + 1)
                            if kind == "model":
                                return ("model", (path, stmt), prefix)
                            if kind in ("model-own", "model-glob"):
                                return ("model", (path, stmt), absolute if kind == "model-own" else MODEL_CRATE_NAME)
                            return (kind, none, none)
                    if in_model:
                        return ("model", (path, stmt), absolute)  # the model crate has one `Change`
                    return ("unknown", none, none)
            if name in sc.change_aliases:  # `type X = Change;` here: X is whatever Change is here
                return self.resolve(path, s, "Change", depth + 1)
            if name in sc.decls:
                if in_model and path == self.root / CHANGE_FILE and name == "Change":
                    return ("model-own", none, none)
                return ("other", none, none)
            for stmt in sc.uses:
                for e in stmt.entries:
                    if not e.glob:
                        continue
                    if e.segs and e.segs[0] == MODEL_CRATE_NAME:
                        return ("model-glob", none, none)
                    if e.segs and e.segs[-1] == "Change":
                        continue  # a variant glob: reported elsewhere
                    if e.segs and e.segs[0] in ("crate", "super", "self"):
                        target = self.relative_target(path, s, e.segs, glob=True)
                        if target is not None:
                            kind, _, prefix = self.resolve(target[0], target[1], name, depth + 1)
                            absolute = "::".join(e.segs)
                            if kind == "model":
                                return ("model", (path, stmt), prefix)
                            if kind == "model-own":
                                return ("model", (path, stmt), absolute)
                            if kind == "model-glob":
                                return ("model", (path, stmt), MODEL_CRATE_NAME)
                            if kind != "unknown":
                                return (kind, none, none)
            if sc.is_mod or sc.parent is None:
                break
            s = sc.parent
        if s == 0:  # an included file is part of the module that includes it
            decl = self.declarations().get(path.resolve())
            if decl is not None and decl[2] == "include":
                # the included file IS that module, so its anchor is ours
                return self.resolve(decl[0], decl[1], name, depth + 1)
        return ("unknown", none, none)

    def alias_status(self, path, scope, name, depth=0):
        """What `name` is where a site stands, when it is written as the first
        segment of a path: 'model' when it is `use pbps_model as name;` in this
        scope or the nearest one that binds it, 'foreign' when that binding or a
        declaration is something else, 'unbound' when nothing here says."""
        if depth > 8:
            return "unbound"
        f = self.facts(path)
        if isinstance(f, LexError):
            return "unbound"
        s = scope
        while True:
            sc = f.scopes[s]
            for stmt in sc.uses:
                for e in stmt.entries:
                    if not e.glob and (e.alias or e.segs[-1]) == name:
                        return "model" if e.segs == (MODEL_CRATE_NAME,) else "foreign"
            if name in sc.decls:
                return "foreign"
            for stmt in sc.uses:
                for e in stmt.entries:
                    if e.glob and e.segs and e.segs[0] in ("crate", "super", "self"):
                        target = self.relative_target(path, s, e.segs, glob=True)
                        if target is not None:
                            found = self.alias_status(target[0], target[1], name, depth + 1)
                            if found != "unbound":
                                return found
            if sc.is_mod or sc.parent is None:
                return "unbound"
            s = sc.parent

    def relative_target(self, path, scope, segs, glob=False):
        """The (file, scope index) a `super::` / `crate::` / `self::` path
        leads to, for a path whose last segment is a name in that module."""
        mods = list(segs) if glob else list(segs[:-1])
        cur_path, cur_scope = path, self.module_scope(self.facts(path), scope)
        for s in mods:
            if s == "self":
                continue
            if s == "super":
                up = self.super_of(cur_path, cur_scope)
                if up is None:
                    return None
                cur_path, cur_scope = up
            elif s == "crate":
                rf = self.crate_root_file(cur_path)
                if rf is None or not rf.exists():
                    return None
                cur_path, cur_scope = rf, 0
            else:
                return None  # a named submodule: not followed
        if isinstance(self.facts(cur_path), LexError):
            return None
        return cur_path, cur_scope

    # ---- scanning a file ---------------------------------------------------
    def report(self, f, offset, reason, text=None):
        line = f.line(offset)
        if text is None:
            text = f.src[f.line_starts[line - 1] : f.line_starts[line] - 1 if line < len(f.line_starts) else None].strip()
        self.reports.append((self.rel(f.path), line, reason, text))

    def scan(self, path):
        f = self.facts(path)
        if isinstance(f, LexError):
            if any(k in path.read_text() for k in self.kinds):
                self.reports.append((self.rel(path), 1, f"file skipped: {f}", ""))
            return
        toks = f.toks
        kinds = self.kinds
        _, in_model, _ = self.crate_info(path)
        names = {"Change"}
        for stmt in f.uses:
            for e in stmt.entries:
                if not e.glob and e.segs[-1] == "Change" and e.alias:
                    names.add(e.alias)
        names |= set(f.type_alias)
        edits = self.edits.setdefault(path, [])
        needs_inner = []  # (site token offset, resolution)
        variant_imports = set()

        # variant imports: `use Change::*`, `use Change::{K}`, `use ...::Change::K`
        for stmt in f.uses:
            for e in stmt.entries:
                segs = e.segs
                if len(segs) >= 1 and e.glob and segs[-1] in names:
                    variant_imports.add("*")
                    self.report(f, stmt.use_start, "glob import of Change's variants", None)
                elif len(segs) >= 2 and segs[-2] in names and segs[-1] in kinds:
                    variant_imports.add(segs[-1])
                    self.report(f, stmt.use_start, f"imports the variant {segs[-1]}", None)

        for i, t in enumerate(toks):
            if t.kind != "ident" or i + 2 >= len(toks):
                continue
            nxt, kind_tok = toks[i + 1], toks[i + 2]
            if nxt.text != "::" or kind_tok.kind != "ident":
                continue
            word = t.text
            if word == "Self":
                if f.impl_change[i] and kind_tok.text in kinds:
                    self.report(f, t.start, f"`Self::{kind_tok.text}` inside `impl Change`")
                continue
            if word not in names:
                continue
            if kind_tok.text.startswith("$"):
                self.report(f, t.start, "metavariable in the variant position")
                continue
            if kind_tok.text not in kinds:
                continue
            self.site(f, path, in_model, i, names, edits, needs_inner)

        # bare kind names after a variant import
        if variant_imports:
            bare = kinds if "*" in variant_imports else variant_imports
            for i, t in enumerate(toks):
                if t.kind == "ident" and t.text in bare:
                    before = toks[i - 1].text if i else ""
                    after = toks[i + 1].text if i + 1 < len(toks) else ""
                    if before in ("::", ".") or after == "::":
                        continue
                    if any(t.start >= s.use_start and t.end <= s.end for s in f.uses):
                        continue
                    self.report(f, t.start, f"bare `{t.text}` after a variant import")

        # mentions in comments and strings
        self.scan_mentions(f)

        self.finish_imports(f, path, needs_inner)

    def scan_mentions(self, f):
        pat = re.compile(r"\bChange::(" + "|".join(sorted(self.kinds)) + r")\b")
        for c in f.comments:
            if pat.search(f.src[c.start : c.end]):
                if c.doc:
                    self.report(f, c.start, "doc comment names a kind of the family")
                else:
                    self.comment_mentions += 1
        for t in f.toks:
            if t.kind == "str" and pat.search(t.text):
                self.notes.append((self.rel(f.path), f.line(t.start), "string literal names a kind of the family"))

    def path_start(self, f, i):
        """Index of the first token of the path ending at token i (a name)."""
        toks = f.toks
        j = i
        while j >= 2 and toks[j - 1].text == "::" and toks[j - 2].kind == "ident":
            j -= 2
        if j >= 1 and toks[j - 1].text == "::":
            j -= 1  # leading `::`
        return j

    def site(self, f, path, in_model, i, names, edits, needs_inner):
        toks = f.toks
        src = f.src
        name_tok, kind_tok = toks[i], toks[i + 2]
        ctx = f.ctx_of[i]
        ps = self.path_start(f, i)
        qualified = ps < i
        qual_text = ""
        if qualified:
            # every join of the path must be written without a gap
            for j in range(ps, i + 2):
                if toks[j].end != toks[j + 1].start:
                    self.report(f, toks[ps].start, "path is split by whitespace or a comment")
                    return
            qual_toks = toks[ps:i]
            first = next(t for t in qual_toks if t.text != "::")
            s0 = first.text
            qual_text = src[toks[ps].start : name_tok.start]
        elif toks[i + 1].end != kind_tok.start or name_tok.end != toks[i + 1].start:
            self.report(f, name_tok.start, "path is split by whitespace or a comment")
            return
        # which `Change` is this?
        resolved = None
        if qualified:
            _, in_model_file, _ = self.crate_info(path)
            if name_tok.text != "Change" and name_tok.text not in names:
                return
            alias = None if s0 in ("crate", "$crate", "super", "self", MODEL_CRATE_NAME) else self.alias_status(path, f.scope_of[i], s0)
            if s0 == MODEL_CRATE_NAME or alias == "model":
                pass
            elif s0 in ("crate", "$crate", "super", "self") and in_model_file:
                pass
            elif s0 in ("crate", "$crate", "super", "self"):
                self.skipped_other[self.rel(path)] += 1
                return
            elif alias == "foreign":
                self.report(f, toks[ps].start, f"`{s0}` is not the model crate where this is written: it names another item")
                return
            else:
                self.report(f, toks[ps].start, f"unrecognised qualifier `{s0}` before `{name_tok.text}`")
                return
        else:
            kind, anchor, prefix = self.resolve(path, f.scope_of[i], name_tok.text)
            resolved = (kind, anchor, prefix)
            if kind == "other":
                self.skipped_other[self.rel(path)] += 1
                return
            if kind == "unknown":
                self.report(f, name_tok.start, f"cannot tell which `{name_tok.text}` this is")
                return
        # context: macros
        if ctx is not None:
            if ctx[0] == "macro_rules":
                if not (qualified and s0 == "$crate"):
                    self.report(f, name_tok.start, f"inside the macro_rules! body of `{ctx[1]}`")
                    return
            elif ctx[0] == "macro" and (ctx[1] not in TRANSPARENT_MACROS or ctx[1] in f.user_macros):
                self.report(f, name_tok.start, f"inside an invocation of the macro `{ctx[1]}!`")
                return
        # the inner enum must be what this file means by its name
        if not qualified and resolved[0] == "model":
            status = self.inner_status(path, f.scope_of[i], resolved[2])
            if status == "collision":
                self.report(
                    f,
                    name_tok.start,
                    f"`{self.inner}` is already a different item here (imported or declared); not the model's enum",
                )
                return
            if status == "present":
                resolved = ("present", None, None)
        # payload group
        shape = self.shapes.get(kind_tok.text)
        after = i + 3
        group_end = None
        if shape == "unit":
            group_end = kind_tok.end
        else:
            want = "{" if shape == "struct" else "(" if shape == "tuple" else None
            if after < len(toks) and toks[after].text in ("{", "(") and (want is None or toks[after].text == want):
                group_end = toks[f.close_of[after]].end
            else:
                self.report(f, name_tok.start, f"`{kind_tok.text}` used without its payload group")
                return
        # the rewrite: two insertions around the variant and its group
        inner_qual = qual_text  # qualifier kept for the inner enum
        edits.append(Edit(kind_tok.start, 1, f"{self.outer}({inner_qual}{self.inner}::"))
        edits.append(Edit(group_end, 0, ")"))
        self.rewrites[self.rel(path)] += 1
        if not qualified and resolved[0] == "model":
            needs_inner.append((name_tok.start, resolved))

    def inner_status(self, path, scope, prefix):
        """Where a site stands, what the inner enum's name means: 'present' when
        it is already imported there from the model (the path is the one a
        mirrored import would write, or begins at the model crate), 'collision'
        when something else holds the name, 'free' when nothing does."""
        f = self.facts(path)
        _, in_model, _ = self.crate_info(path)
        s = scope
        while True:
            sc = f.scopes[s]
            for stmt in sc.uses:
                for e in stmt.entries:
                    if e.glob or (e.alias or e.segs[-1]) != self.inner:
                        continue
                    ours = e.segs[-1] == self.inner and (
                        e.alias in (None, self.inner)
                        and (
                            e.segs[0] == MODEL_CRATE_NAME
                            or self.alias_status(path, s, e.segs[0]) == "model"
                            or "::".join(e.segs[:-1]) == (prefix or "")
                            or (in_model and e.segs[0] in ("crate", "super", "self"))
                        )
                    )
                    return "present" if ours else "collision"
            if self.inner in sc.decls:
                return "present" if in_model and path == self.root / CHANGE_FILE else "collision"
            if sc.is_mod or sc.parent is None:
                return "free"
            s = sc.parent

    def finish_imports(self, f, path, needs_inner):
        """The inner enum must be nameable where a bare use needs it: one
        `use <prefix>::Inner;` after the statement that brings `Change` into
        the site's scope, in that scope, so it is used wherever `Change` is."""
        for offset, (kind, anchor, prefix) in needs_inner:
            if anchor is None:
                self.reports.append(
                    (self.rel(path), f.line(offset), f"`{self.inner}` must be imported here; no `use` of Change to extend", "")
                )
                continue
            mpath, stmt = anchor
            if (mpath, stmt.start) not in self.import_edits:
                self.mirror_import(mpath, stmt, prefix)

    def mirror_import(self, mpath, stmt, prefix):
        mf = self.facts(mpath)
        glob = any(e.glob for e in stmt.entries)
        attrs = mf.src[stmt.start : stmt.use_start]
        indent = mf.src[stmt.line_start : stmt.start]
        indent = indent if not indent.strip() else ""
        vis = "" if glob else stmt.vis
        line = f"{attrs}{vis}use {prefix}::{self.inner};"
        eol = mf.src.find("\n", stmt.end)
        rest = mf.src[stmt.end : eol if eol >= 0 else len(mf.src)]
        text = ("\n" + indent + line) if rest.strip() == "" else (" " + line)
        self.import_edits[(mpath, stmt.start)] = (stmt.end, text)
        if vis.strip():
            self.notes.append((self.rel(mpath), mf.line(stmt.use_start), "re-exports Change; the inner enum is re-exported too"))

    # ---- applying ---------------------------------------------------------
    def apply(self, write):
        by_file = {}
        for path, edits in self.edits.items():
            if edits:
                by_file.setdefault(path, []).extend(edits)
        for (mpath, _), edit in self.import_edits.items():
            if edit is not None:
                by_file.setdefault(mpath, []).append(Edit(edit[0], 2, edit[1]))
        changed = []
        for path, edits in sorted(by_file.items()):
            src = path.read_text()
            out, last = [], 0
            for e in sorted(edits, key=lambda e: (e.pos, e.order)):
                out.append(src[last : e.pos])
                out.append(e.text)
                last = e.pos
            out.append(src[last:])
            new = "".join(out)
            if new != src:
                changed.append(path)
                if write:
                    path.write_text(new)
        return changed


def move_enum(root, family, outer, inner):
    """Move the kinds of one family out of `enum Change` into `enum INNER`."""
    kinds, _ = read_family(root, family, outer, inner)
    path = root / CHANGE_FILE
    src = path.read_text()
    toks, _ = lex(src)
    close = match_brackets(toks)
    i = next(
        (k for k, t in enumerate(toks) if t.text == "enum" and toks[k + 1].text == "Change" and toks[k + 2].text == "{"),
        None,
    )
    if i is None or any(t.text == "enum" and toks[k + 1].text == inner for k, t in enumerate(toks)):
        raise SystemExit(f"`enum Change` is missing or `enum {inner}` exists already in {CHANGE_FILE}")
    end = close[i + 2]

    def span(name):
        for k in range(i + 3, end):
            if toks[k].text == name and toks[k + 1].text in ("{", "(", ",") and toks[k - 1].text in (",", "}", "{", "]", ")"):
                if toks[k - 1].text == "{" and k - 1 != i + 2:
                    continue
                first = k
                while toks[first - 1].text == "]":  # attributes of the variant
                    opened = next(a for a, b in close.items() if b == first - 1)
                    first = opened - 1
                start = src.rfind("\n", 0, toks[first].start) + 1
                while True:  # doc comment lines above it
                    prev = src.rfind("\n", 0, start - 1) + 1
                    if src[prev:start].strip().startswith("///"):
                        start = prev
                    else:
                        break
                last = close[k + 1] if toks[k + 1].text in ("{", "(") else k  # a unit variant has no group
                stop = toks[last + 1].end if toks[last + 1].text == "," else toks[last].end
                return start, stop
        raise SystemExit(f"the kind {name} is not a variant of `enum Change` in {CHANGE_FILE}")

    spans = sorted(span(n) for n in kinds)
    block = "".join(src[a:b] + "\n" for a, b in spans)
    out, last = [], 0
    for a, b in spans:
        out.append(src[last:a])
        last = b + 1
    out.append(src[last:])
    new = "".join(out)
    toks2, _ = lex(new)
    close2 = match_brackets(toks2)
    i2 = next(k for k, t in enumerate(toks2) if t.text == "enum" and toks2[k + 1].text == "Change" and toks2[k + 2].text == "{")
    e2 = close2[i2 + 2]
    variant = (
        f"    /// The kinds of the `{family}` family (#2790). Serialised as the inner kind alone:\n"
        f"    /// each change stays one flat object tagged by `op`.\n"
        f"    #[serde(untagged)]\n    {outer}({inner}),\n"
    )
    enum = (
        f"\n/// The kinds of change of the `{family}` family (#2790): the enum `change_family!({family})`\n"
        f"/// names. [`Change::{outer}`] holds one and serialises as it does, so a saved plan\n"
        f"/// keeps its shape.\n"
        f"#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, JsonSchema)]\n"
        f'#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]\n'
        f"pub enum {inner} {{\n{block}}}\n"
    )
    cut = toks2[e2].start
    new = new[:cut] + variant + new[cut : toks2[e2].end] + "\n" + enum.rstrip("\n") + "\n" + new[toks2[e2].end :]
    path.write_text(new)
    lib = root / "crates/pbps-model/src/lib.rs"
    text = lib.read_text()
    if "pub use change::{\n" not in text:
        raise SystemExit("lib.rs has no `pub use change::{` list to extend; add the re-export by hand")
    lib.write_text(text.replace("pub use change::{\n", f"pub use change::{{\n    {inner},\n", 1))
    return kinds


def rust_files(root):
    out = []
    for p in sorted(root.rglob("*.rs")):
        rel = p.relative_to(root).parts
        if any(part in SKIP_DIRS or part.startswith(".") for part in rel[:-1]):
            continue
        out.append(p)
    return out


def run(root, family, outer, inner, write, out=sys.stdout):
    kinds, shapes = read_family(root, family, outer, inner)
    project = Project(root, family, outer, inner, kinds, shapes)
    kind_pat = re.compile(r"\b(" + "|".join(sorted(kinds)) + r")\b|::\s*\$")
    for path in rust_files(root):
        text = path.read_text()
        if not kind_pat.search(text):
            continue
        project.scan(path)
    changed = project.apply(write)
    verb = "rewrote" if write else "would rewrite"
    for rel, line, reason, text in sorted(set(project.reports)):
        print(f"{rel}:{line}: REPORT {reason}: {text}", file=out)
    for rel, line, reason in sorted(set(project.notes)):
        print(f"{rel}:{line}: NOTE {reason}", file=out)
    for path in changed:
        rel = project.rel(path)
        print(f"{rel}: {verb} {project.rewrites[rel]} site(s)", file=out)
    sites = sum(project.rewrites.values())
    print(
        f"{family}: {verb} {sites} site(s) in {len(changed)} file(s); "
        f"{sum(1 for v in project.import_edits.values() if v)} import line(s); "
        f"{len(set(project.reports))} reported; {len(set(project.notes))} note(s); "
        f"{project.comment_mentions} plain-comment mention(s) left; "
        f"{sum(project.skipped_other.values())} site(s) of another `Change` left",
        file=out,
    )
    return project


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("family", help="family name as in change_family!(...), e.g. extensions")
    ap.add_argument("outer", help="variant of Change that holds the family, e.g. Extension")
    ap.add_argument("inner", help="the family's enum, e.g. ExtensionChange")
    ap.add_argument("root", help="repository root")
    ap.add_argument("--check", action="store_true", help="list what would change; write nothing")
    ap.add_argument("--move-enum", action="store_true", help="move the family's variants into their own enum, then stop")
    ap.add_argument("--fail-on-report", action="store_true", help="exit 1 when anything was reported")
    args = ap.parse_args(argv)
    if args.move_enum:
        kinds = move_enum(Path(args.root).resolve(), args.family, args.outer, args.inner)
        print(f"moved {len(kinds)} kinds into {args.inner}: {', '.join(kinds)}")
        return 0
    project = run(Path(args.root).resolve(), args.family, args.outer, args.inner, write=not args.check)
    return 1 if args.fail_on_report and project.reports else 0


if __name__ == "__main__":
    sys.exit(main())
