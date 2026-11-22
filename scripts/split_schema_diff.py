#!/usr/bin/env python3
"""Move production items out of `crates/pbps-diff/src/schema_diff.rs` into
`schema_diff/<module>.rs`, and check that nothing but the move happened
(issue #3003, step 4 of #2779). Python 3.12, standard library only.

    scripts/split_schema_diff.py                    apply the plan to the tree
    scripts/split_schema_diff.py --check BASE_FILE  the move check
    scripts/split_schema_diff.py --map [BASE_FILE]  old line range -> module
                                                    (--base-ref REF: leave out what
                                                    REF's modules already held)

Why a script and not a line-range plan: other branches edit this file while the
split lands. The script reads the tree it is given and never a line number, so
a branch re-runs it on its own tip and the three-way merge sees only that
branch's own edits. It is removed by the last PR of #3003.

The plan (`split_schema_diff.plan.json`) maps a module name to item names. The
reserved keys `_docs` (the `//!` line of each module), `_pub_super` and
`_no_reexport` (see below) are not modules. A name in the plan that the tree does not have is an
error, never a skip.

A module name `group/part` is a module one level down: the file
`schema_diff/group/part.rs`, declared by `schema_diff/group.rs` (the repository
writes `foo.rs` beside `foo/`, not `foo/mod.rs`). The plan gives `group` a line
in `_docs` and no item list; a flat module of that name is an error, and so is
a name nested deeper. The script owns `group.rs`: it holds the `//!` line and
one `pub(super) mod part;` line per planned part, and nothing else (a file that
holds an item is an error). The parent file declares `mod group;` and imports
`group::part::item`, so the parent stays the one place every module reaches
the others through, as for a flat module: a nested module opens with
`use super::super::*;`, which is the parent's namespace, its imports included.
No re-export through `group.rs` is needed, and no sibling imports another by
path. The price is the visibility of a private item that anything else reads:
the parent and the sibling modules sit one level above the item's `group`, so
it gets `pub(in super::super)` where a flat module's gets `pub(super)`.

How an item is named (the name the plan lists):

    fn name, struct Name, enum Name, union Name, trait Name, const NAME,
    static NAME, type Name         -> `name`
    impl X, impl<'a> X<'a>, impl T for X
                                   -> `X`, the self type's last path segment.
                                      An impl goes with its type, wherever in
                                      the file it stands. An impl of a type
                                      the file does not declare is named by
                                      `impl T for X`, T and X each the last
                                      path segment, generics left out.

An item is cut together with the doc comments, comments and attributes just
above it. Items keep their text. A planned item moves to its module in source
order; modules that already exist only gain items (appended, in source order),
so a second run changes nothing. Items the plan does not name stay in place in
their order. `use` and `mod` lines are not items: the parent gains one `mod`
line per module and one named import per moved item that is not private.

Visibility, the one edit an item gets:

- An already `pub` or `pub(crate)` item keeps its visibility and is re-exported
  from the parent by a named `pub use` / `pub(crate) use`, so no path outside
  the file changes.
- A private item gets `pub(super)` (`pub(in super::super)` in a nested module)
  exactly when its name occurs as a word (not after a `.`) outside its new
  module: in the parent's remaining items, in another module (a sibling in the
  same group included), or in a test file the parent declares. The parent then
  imports it by name, which is how every other module and the tests reach it
  through their `use super::*;`. A name that only a test file uses gets a
  `#[cfg(test)]` import, so the build without tests has no unused import.
- A struct field or an inherent impl method cannot be found by name, so the
  plan lists them in `_pub_super` as `Type.field` and `Type::method`.
- A `pub(crate)` item that nothing outside the file reads has no re-export (it
  would be an unused import); the plan lists it in `_no_reexport`, and inside
  the file it is imported as a private item is: not at all, plainly, or only
  in the test build. The script cannot see who reads an item from another
  file, so a `pub(crate)` item not listed there is always re-exported. A name there
  must be a `pub(crate)` item the plan moves, and the entry goes when something
  starts to read the item from another file.

The check (`--check BASE_FILE`, a saved copy of the file before the move) lexes
both trees, cuts them into items and requires: every base item appears exactly
once in the new tree; its tokens equal the base item's after deleting an
inserted `pub(super)` (or `pub(in super::super)`) at an item head, a
struct-field head or an impl-method head, and an inserted trailing `,` before
the `)` of a signature; items left in the parent and items of each module keep
their order; no extra item exists; the `use`/`mod` lines are the only other
difference and are printed. The modules are found by following the `mod` lines,
a group's included. With `--base-ref REF` the modules and test files as they
were at REF count too (a later PR's check), and the test files must be
byte-identical to REF's.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

DEFAULT_FILE = "crates/pbps-diff/src/schema_diff.rs"
DEFAULT_PLAN = Path(__file__).with_suffix(".plan.json")
RESERVED = ("_docs", "_pub_super", "_no_reexport")
PUB_SUPER = ["pub", "(", "super", ")"]
_MODULE_NAME = r"[a-z][a-z0-9_]*"


class SplitError(Exception):
    pass


# ----------------------------------------------------------------- lexer


@dataclass(slots=True)
class Tok:
    kind: str  # id, num, life, str, doc, com, p
    text: str
    start: int
    end: int


_RAW = re.compile(r'(?:b|c)?r(#*)"')
_IDENT = re.compile(r"[^\W\d]\w*")
_NUM = re.compile(r"[0-9][0-9A-Za-z_]*(?:\.[0-9][0-9A-Za-z_]*)?")


def tokenize(text: str) -> list[Tok]:
    """Comments (doc comments apart), strings, raw strings, chars and lifetimes
    are single tokens, so a brace inside any of them is never a brace."""
    toks: list[Tok] = []
    i, n = 0, len(text)
    while i < n:
        c = text[i]
        if c.isspace():
            i += 1
        elif text.startswith("//", i):
            j = text.find("\n", i)
            j = n if j < 0 else j
            body = text[i:j].rstrip()
            doc = (body.startswith("///") and not body.startswith("////")) or body.startswith("//!")
            toks.append(Tok("doc" if doc else "com", body, i, i + len(body)))
            i = j
        elif text.startswith("/*", i):
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            if depth:
                raise SplitError(f"unterminated block comment at offset {i}")
            body = text[i:j]
            doc = (body.startswith("/**") and not body.startswith("/***") and body != "/**/") or body.startswith("/*!")
            toks.append(Tok("doc" if doc else "com", body, i, j))
            i = j
        elif c in "bcr" and (m := _RAW.match(text, i)):
            closing = '"' + m.group(1)
            j = text.find(closing, m.end())
            if j < 0:
                raise SplitError(f"unterminated raw string at offset {i}")
            j += len(closing)
            toks.append(Tok("str", text[i:j], i, j))
            i = j
        elif c == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            if j >= n:
                raise SplitError(f"unterminated string at offset {i}")
            toks.append(Tok("str", text[i : j + 1], i, j + 1))
            i = j + 1
        elif c == "'":
            if text.startswith("\\", i + 1):
                j = text.find("'", i + 3)
                if j < 0:
                    raise SplitError(f"unterminated char literal at offset {i}")
                toks.append(Tok("str", text[i : j + 1], i, j + 1))
                i = j + 1
            elif text.startswith("'", i + 2):
                toks.append(Tok("str", text[i : i + 3], i, i + 3))
                i += 3
            elif m := _IDENT.match(text, i + 1):
                toks.append(Tok("life", text[i : m.end()], i, m.end()))
                i = m.end()
            else:
                raise SplitError(f"cannot read a quote at offset {i}")
        elif m := _IDENT.match(text, i):
            toks.append(Tok("id", m.group(), i, m.end()))
            i = m.end()
        elif m := _NUM.match(text, i):
            toks.append(Tok("num", m.group(), i, m.end()))
            i = m.end()
        else:
            toks.append(Tok("p", c, i, i + 1))
            i += 1
    return toks


def pair_delims(toks: list[Tok]) -> dict[int, int]:
    """Opening -> closing index for every (), [] and {} in code."""
    pairs: dict[int, int] = {}
    stack: list[int] = []
    for i, t in enumerate(toks):
        if t.kind != "p":
            continue
        if t.text in "([{":
            stack.append(i)
        elif t.text in ")]}":
            if not stack or "([{".index(toks[stack[-1]].text) != ")]}".index(t.text):
                raise SplitError(f"unbalanced `{t.text}` at offset {t.start}")
            pairs[stack.pop()] = i
    if stack:
        raise SplitError(f"unbalanced `{toks[stack[-1]].text}` at offset {toks[stack[-1]].start}")
    return pairs


# ----------------------------------------------------------------- items

_KEYWORDS = ("fn", "struct", "enum", "trait", "type", "const", "static", "mod", "use", "impl", "union")
_SEMI_ENDED = ("use", "const", "static", "type")


@dataclass
class Item:
    kind: str  # fn struct enum ... use mod impl trailing
    name: str  # impl: the self type's last path segment
    display: str  # impl: `impl X` or `impl T for X`
    vis: str | None
    lead: int  # token index of the first leading comment or attribute
    head: int  # token index of `pub` or the keyword
    kw: int  # token index of the keyword
    end: int  # one past the last token
    start: int  # text offsets, leading comments and attributes included
    stop: int
    toks: list[Tok]  # the file's tokens
    pairs: dict[int, int]
    text: str

    @property
    def glue(self) -> bool:
        return self.kind in ("use", "mod_decl", "extern_crate", "trailing")

    @property
    def key(self) -> str:
        return f"{self.kind}:{self.display or self.name}"

    @property
    def source(self) -> str:
        return self.text[self.start : self.stop]

    def tokens(self) -> list[str]:
        return [t.text for t in self.toks[self.lead : self.end]]

    def is_test_gated(self) -> bool:
        return "#[cfg(test)]" in "".join(t.text for t in self.toks[self.lead : self.head])


def _skip_attr(toks: list[Tok], pairs: dict[int, int], i: int) -> int | None:
    if toks[i].text == "#" and i + 1 < len(toks):
        j = i + 2 if toks[i + 1].text == "!" else i + 1
        if j < len(toks) and toks[j].text == "[":
            return pairs[j] + 1
    return None


def _skip_angle(toks: list[Tok], i: int) -> int:
    """`toks[i]` is `<`: the index after its `>` (an `->` is not a close)."""
    depth = 0
    while True:
        if toks[i].text == "<":
            depth += 1
        elif toks[i].text == ">" and toks[i - 1].text != "-":
            depth -= 1
        i += 1
        if depth == 0:
            return i


def _impl_names(toks: list[Tok], kw: int) -> tuple[str, str]:
    """The self type's last path segment, and the `impl X` / `impl T for X` text."""
    i = kw + 1
    if toks[i].text == "<":  # the generics of the impl itself
        i = _skip_angle(toks, i)
    parts: list[list[str]] = [[]]
    depth = 0
    while not (toks[i].text in ("{", ";") or (depth == 0 and toks[i].text == "where")):
        t = toks[i].text
        if t == "<":
            depth += 1
        elif t == ">" and toks[i - 1].text != "-":
            depth -= 1
        if depth == 0 and t == "for":
            parts.append([])
        else:
            parts[-1].append(t)
        i += 1

    def head_of(words: list[str]) -> str:
        out, depth = "", 0
        for w in words:
            if w == "<":
                depth += 1
            elif w == ">":
                depth -= 1
            elif depth == 0 and re.fullmatch(r"[^\W\d]\w*", w) and w not in ("mut", "dyn", "const"):
                out = w
        return out

    self_type = head_of(parts[-1])
    shown = "impl " + (head_of(parts[0]) + " for " if len(parts) > 1 else "") + self_type
    return self_type, shown


def _parse_item(text: str, toks: list[Tok], pairs: dict[int, int], lead: int, head: int) -> Item:
    k, vis = head, None
    if toks[k].text == "pub":
        k += 1
        if toks[k].text == "(":
            inner = [t.text for t in toks[k + 1 : pairs[k]]]
            vis = "pub(" + ("in " + "".join(inner[1:]) if inner[:1] == ["in"] else "".join(inner)) + ")"
            k = pairs[k] + 1
        else:
            vis = "pub"
    while True:
        t = toks[k].text
        if t in ("unsafe", "async", "default"):
            k += 1
        elif t == "const" and toks[k + 1].text in ("fn", "unsafe", "async", "extern"):
            k += 1
        elif t == "extern" and toks[k + 1].kind == "str":
            k += 2
        else:
            break
    kw = toks[k].text
    name = display = ""
    if kw == "extern" and toks[k + 1].text == "crate":
        kind, name = "extern_crate", toks[k + 2].text
    elif kw == "macro_rules" and toks[k + 1].text == "!":
        kind, name = "macro", toks[k + 2].text
    elif kw == "union" and toks[k + 1].kind == "id" and toks[k + 2].text in ("{", "<"):
        kind, name = "union", toks[k + 1].text
    elif kw in _KEYWORDS and kw != "union":
        kind = kw
        if kw == "impl":
            name, display = _impl_names(toks, k)
        elif kw != "use":
            name = toks[k + 2].text if toks[k + 1].text == "mut" else toks[k + 1].text
    else:
        raise SplitError(f"cannot read the item at offset {toks[head].start}: `{toks[k].text}`")
    # Where the item ends: a `;`, or the `}` of the first `{`, outside any bracket.
    j = k
    while True:
        if j >= len(toks):
            raise SplitError(f"item at offset {toks[head].start} does not end")
        t = toks[j].text if toks[j].kind == "p" else ""
        if t == ";":
            end = j + 1
            break
        if t in ("(", "["):
            j = pairs[j]
        elif t == "{":
            if kind in _SEMI_ENDED:
                j = pairs[j]
            else:
                end = pairs[j] + 1
                break
        j += 1
    if kind == "mod" and toks[end - 1].text == ";":
        kind = "mod_decl"
    return Item(kind, name, display, vis, lead, head, k, end, toks[lead].start, toks[end - 1].end, toks, pairs, text)


def cut_items(text: str) -> list[Item]:
    """Every top-level item, `use` and `mod name;` line, each with the comments
    and attributes above it. Trailing comments become a `trailing` item."""
    toks = tokenize(text)
    pairs = pair_delims(toks)
    items: list[Item] = []
    i, n = 0, len(toks)
    while i < n:
        lead = i
        while i < n:
            if toks[i].kind in ("doc", "com"):
                i += 1
            elif (after := _skip_attr(toks, pairs, i)) is not None:
                i = after
            else:
                break
        if i >= n:
            items.append(Item("trailing", "", "", None, lead, n, n, n, toks[lead].start, toks[n - 1].end, toks, pairs, text))
            break
        item = _parse_item(text, toks, pairs, lead, i)
        items.append(item)
        i = item.end
    return items


def members(item: Item) -> list[tuple[str, int, bool, str]]:
    """(name, head token index, has visibility, owner type) of a braced struct's
    fields and of an impl's functions, consts and types."""
    toks, pairs = item.toks, item.pairs
    if item.kind not in ("struct", "impl"):
        return []
    open_ = next((j for j in range(item.kw, item.end) if toks[j].text == "{"), None)
    if open_ is None:
        return []
    close = pairs[open_]
    out = []
    i = open_ + 1
    while True:
        while i < close:
            if toks[i].kind in ("doc", "com"):
                i += 1
            elif (after := _skip_attr(toks, pairs, i)) is not None:
                i = after
            else:
                break
        if i >= close:
            return out
        head, has_vis = i, False
        if toks[i].text == "pub":
            has_vis = True
            i = pairs[i + 1] + 1 if toks[i + 1].text == "(" else i + 1
        if item.kind == "struct":
            if toks[i + 1].text == ":":
                out.append((toks[i].text, head, has_vis, item.name))
            while i < close and toks[i].text != ",":
                i = pairs[i] + 1 if toks[i].text in ("(", "[", "{") else i + 1
            i += 1
        else:
            j = i
            while toks[j].text in ("unsafe", "async", "default", "extern") or toks[j].kind == "str" or (
                toks[j].text == "const" and toks[j + 1].text in ("fn", "unsafe", "async", "extern")
            ):
                j += 1
            if toks[j].text in ("fn", "const", "type"):
                out.append((toks[j + 1].text, head, has_vis, item.name))
            while i < close:
                if toks[i].text == ";":
                    i += 1
                    break
                if toks[i].text == "{":
                    i = pairs[i] + 1
                    break
                i = pairs[i] + 1 if toks[i].text in ("(", "[") else i + 1


def signature_closers(item: Item) -> set[int]:
    """Token indices (relative to the item's first token) of the `)` that
    closes a fn parameter list: the one place a re-wrapped signature gains a
    comma."""
    toks, pairs = item.toks, item.pairs
    heads = [item.kw] if item.kind == "fn" else [h for _, h, _, _ in members(item)] if item.kind == "impl" else []
    out = set()
    for h in heads:
        j = h
        while j < item.end and toks[j].text != "fn":
            j += 1
        if j >= item.end:
            continue
        j += 2
        if toks[j].text == "<":
            j = _skip_angle(toks, j)
        out.add(pairs[j] - item.lead)
    return out


def next_start(item: Item, size: int) -> int:
    """Where the next token starts: what to cut up to when an item is removed."""
    return item.toks[item.end].start if item.end < len(item.toks) else size


def line_of(text: str, offset: int) -> int:
    return text.count("\n", 0, offset) + 1


# ----------------------------------------------------------------- plan


def load_plan(path: Path) -> tuple[dict[str, list[str]], dict[str, str], list[str], list[str]]:
    raw = json.loads(path.read_text())
    docs, pub_super = raw.get("_docs", {}), raw.get("_pub_super", [])
    no_reexport = raw.get("_no_reexport", [])
    modules = {k: v for k, v in raw.items() if k not in RESERVED}
    seen: dict[str, str] = {}
    for mod, names in modules.items():
        if not re.fullmatch(f"{_MODULE_NAME}(/{_MODULE_NAME})?", mod):
            raise SplitError(f"plan: `{mod}` is not a module name (a group holds modules one level down only)")
        for need in dict.fromkeys([_top(mod), mod]):
            if need not in docs:
                raise SplitError(f"plan: module `{need}` has no line in `_docs`")
        if "/" in mod and _top(mod) in modules:
            raise SplitError(f"plan: `{_top(mod)}` is a module with items and also the group of `{mod}`")
        for name in names:
            if name in seen:
                raise SplitError(f"plan: `{name}` is listed for `{seen[name]}` and for `{mod}`")
            seen[name] = mod
    return modules, docs, pub_super, no_reexport


def _top(mod: str) -> str:
    """The module the parent declares: `group` for `group/part`."""
    return mod.split("/")[0]


def _groups(modules: dict[str, list[str]]) -> dict[str, list[str]]:
    """Each group's parts, sorted; a flat module is not a group."""
    out: dict[str, list[str]] = {}
    for mod in modules:
        if "/" in mod:
            out.setdefault(_top(mod), []).append(mod.split("/")[1])
    return {g: sorted(parts) for g, parts in out.items()}


def _super_vis(mod: str) -> str:
    """The narrowest visibility that reaches the parent file from `mod`."""
    return "pub(in " + "::".join(["super"] * (mod.count("/") + 1)) + ")" if "/" in mod else "pub(super)"


def _matches(item: Item, name: str) -> bool:
    return not item.glue and item.kind != "trailing" and (item.name == name or item.display == name)


def _is_cfg_test_mod(item: Item) -> bool:
    return item.kind == "mod_decl" and item.is_test_gated()


def _idents(items: list[Item]) -> set[str]:
    out = set()
    for it in items:
        if it.kind in ("use", "trailing"):
            continue
        for k in range(it.lead, it.end):
            t = it.toks[k]
            if t.kind == "id" and not (k > 0 and it.toks[k - 1].text == "."):
                out.add(t.text)
    return out


def _file_idents(path: Path) -> set[str]:
    toks = tokenize(path.read_text())
    return {t.text for i, t in enumerate(toks) if t.kind == "id" and not (i > 0 and toks[i - 1].text == ".")}


# ----------------------------------------------------------------- apply


def _header(mod: str, docs: dict[str, str]) -> str:
    return f"//! {docs[mod]}\n\nuse {'super::' * (mod.count('/') + 1)}*;\n"


def _group_file(group: str, parts: list[str], docs: dict[str, str]) -> str:
    return f"//! {docs[group]}\n\n" + "".join(f"pub(super) mod {part};\n" for part in parts)


def _with_visibility(text: str, used_outside: set[str], forced: list[str], vis: str = "pub(super)") -> str:
    """Insert `pub(super) ` (`vis`) at the head of each private item used
    outside the module, and at each listed field or method."""
    edits: list[int] = []
    for it in cut_items(text):
        if it.glue or it.kind == "impl":
            if it.kind == "impl":
                edits += [h for m, h, vis, owner in members(it) if not vis and f"{owner}::{m}" in forced]
            continue
        if it.vis is None and (it.name in used_outside or it.name in forced):
            edits.append(it.head)
        if it.kind == "struct":
            edits += [h for m, h, vis, owner in members(it) if not vis and f"{owner}.{m}" in forced]
    toks = tokenize(text)
    for h in sorted(set(edits), reverse=True):
        off = toks[h].start
        text = text[:off] + vis + " " + text[off:]
    return text


def _managed(item: Item, modules: set[str]) -> bool:
    if item.kind == "mod_decl":
        return not item.is_test_gated() and item.name in modules
    if item.kind == "use" and item.toks[item.kw + 1].kind == "id":
        return item.toks[item.kw + 1].text in modules and item.toks[item.kw + 2].text == ":"
    return False


def apply(root: Path, parent_rel: str, plan_path: Path) -> list[str]:
    """Plan the whole move, then write; returns what was written."""
    modules, docs, pub_super, no_reexport = load_plan(plan_path)
    parent_path = root / parent_rel
    mod_dir = parent_path.with_suffix("")
    text = parent_path.read_text()
    items = cut_items(text)
    existing = {m: (mod_dir / f"{m}.rs").read_text() if (mod_dir / f"{m}.rs").is_file() else None for m in modules}
    in_module = {m: cut_items(t) for m, t in existing.items() if t is not None}

    moved: dict[str, list[Item]] = {m: [] for m in modules}
    for mod, names in modules.items():
        for name in names:
            here = [it for it in items if _matches(it, name)]
            there = [it for m, its in in_module.items() for it in its if _matches(it, name)]
            elsewhere = [m for m, its in in_module.items() if m != mod and any(_matches(it, name) for it in its)]
            if not here and not there:
                raise SplitError(f"the plan names `{name}` for `{mod}`, and the tree has no such item")
            if elsewhere:
                raise SplitError(f"`{name}` is planned for `{mod}` but stands in `{elsewhere[0]}`")
            if any(it.kind != "impl" for it in here) and any(it.kind != "impl" for it in there):
                raise SplitError(f"`{name}` stands both in the parent and in `{mod}`")
            moved[mod] += here
    for mod in moved:
        moved[mod].sort(key=lambda it: it.start)

    gone = sorted((it for its in moved.values() for it in its), key=lambda it: it.start)
    parent_text = text
    for it in reversed(gone):
        parent_text = parent_text[: it.start] + parent_text[next_start(it, len(text)) :]
    if gone and gone[-1].end >= len(gone[-1].toks):  # the last item went: keep one final newline
        parent_text = parent_text.rstrip("\n") + "\n"
    rest = cut_items(parent_text)

    composed: dict[str, str] = {}
    for mod in modules:
        body = existing[mod] if existing[mod] is not None else _header(mod, docs)
        if moved[mod]:
            body = body.rstrip("\n") + "\n\n" + "\n\n".join(it.source for it in moved[mod]) + "\n"
        composed[mod] = body
    parent_decls = [it for it in rest if it.kind == "mod_decl"]
    tests = [mod_dir / f"{it.name}.rs" for it in parent_decls if it.is_test_gated()]

    kept_code = _idents([it for it in rest if not it.glue])
    kept = set(kept_code)
    for path in tests:
        if path.is_file():
            kept |= _file_idents(path)
    per_module = {mod: _idents(cut_items(body)) for mod, body in composed.items()}
    final: dict[str, str] = {}
    read_in_file: set[tuple[str, str]] = set()
    for mod in modules:
        outside = set(kept).union(*(ids for other, ids in per_module.items() if other != mod))
        final[mod] = _with_visibility(composed[mod], outside, pub_super, _super_vis(mod))
        read_in_file |= {(mod, name) for name in outside}
    # a name only a test file reads needs its import only in the test build
    test_only = {
        (mod, name)
        for mod in modules
        for name in kept - set(kept_code).union(*(ids for other, ids in per_module.items() if other != mod))
    }

    parent_text = _parent_glue(parent_text, rest, modules, final, test_only, no_reexport, read_in_file)
    written = []
    for mod, body in final.items():
        path = mod_dir / f"{mod}.rs"
        if existing[mod] != body:
            written.append((path, body))
    for group, parts in _groups(modules).items():
        path = mod_dir / f"{group}.rs"
        body = _group_file(group, parts, docs)
        if path.is_file():
            if held := [it for it in cut_items(path.read_text()) if not it.glue]:
                raise SplitError(f"`{path.name}` is the group of {parts} and holds the item `{held[0].display or held[0].name}`")
            if path.read_text() == body:
                continue
        written.append((path, body))
    if parent_text != text:
        written.append((parent_path, parent_text))
    for path, body in written:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body)
    return [str(p.relative_to(root)) for p, _ in written]


def _parent_glue(
    parent_text: str,
    rest: list[Item],
    modules: dict[str, list[str]],
    final: dict[str, str],
    test_only: set[tuple[str, str]],
    no_reexport: list[str],
    read_in_file: set[tuple[str, str]],
) -> str:
    """Replace the managed `mod` and import lines by those the modules need."""
    names = set(modules)
    tops = {_top(m) for m in names}
    for it in reversed(rest):
        if _managed(it, tops):
            parent_text = parent_text[: it.start] + parent_text[next_start(it, len(parent_text)) :]
    rest = cut_items(parent_text)
    mods = [f"mod {m};" for m in sorted(tops)]
    uses = []
    unexported = set(no_reexport)
    for mod in sorted(names):
        for it in cut_items(final[mod]):
            if it.glue or it.kind == "impl" or it.vis is None:
                continue
            if it.name in unexported:
                if it.vis != "pub(crate)":
                    raise SplitError(f"`_no_reexport` names `{it.name}`, which is not `pub(crate)`")
                unexported.discard(it.name)
                # No file outside reads it, so inside the file it is imported as
                # a private item would be: not at all, plainly, or for tests.
                if (mod, it.name) not in read_in_file:
                    continue
            private = it.vis == _super_vis(mod) or it.name in no_reexport
            shown = "" if private else it.vis + " "
            gate = "#[cfg(test)]\n" if private and (mod, it.name) in test_only else ""
            uses.append(f"{gate}{shown}use {mod.replace('/', '::')}::{it.name};")
    if unexported:
        raise SplitError(f"`_no_reexport` names {sorted(unexported)}, which no module holds as a visible item")
    uses.sort(key=lambda line: line[line.index("use ") + 4 :])
    block = "\n".join(mods) + ("\n\n" + "\n".join(uses) if uses else "")
    last_use = None
    for it in rest:
        if it.kind != "use":
            break
        last_use = it
    if last_use is None:
        raise SplitError("the parent file has no leading `use` lines to put the `mod` lines after")
    follow = next_start(last_use, len(parent_text))
    return parent_text[: last_use.stop] + "\n\n" + block + "\n\n" + parent_text[follow:]


# ----------------------------------------------------------------- check


def _git_show(root: Path, ref: str, rel: str) -> str | None:
    r = subprocess.run(["git", "-C", str(root), "show", f"{ref}:{rel}"], capture_output=True, text=True)
    return r.stdout if r.returncode == 0 else None


def _inserted_visibility(n: list[str], j: int) -> int:
    """How many tokens of an inserted `pub(super)` or `pub(in super::super)`
    start at `n[j]`; 0 when none does."""
    if n[j : j + 4] == PUB_SUPER:
        return 4
    if n[j : j + 4] != ["pub", "(", "in", "super"]:
        return 0
    k = j + 4
    while n[k : k + 3] == [":", ":", "super"]:
        k += 3
    return k + 1 - j if n[k : k + 1] == [")"] else 0


def _align(base: Item, new: Item) -> str | None:
    """None when `new` is `base` plus allowed insertions; else where they differ."""
    b, n = base.tokens(), new.tokens()
    heads = {new.head - new.lead} | {h - new.lead for _, h, _, _ in members(new)}
    closers = signature_closers(new)
    i = j = 0
    while i < len(b) or j < len(n):
        if i < len(b) and j < len(n) and b[i] == n[j]:
            i, j = i + 1, j + 1
        elif j in heads and (size := _inserted_visibility(n, j)):
            j += size
        elif j + 1 in closers and n[j] == "," and i < len(b) and b[i] == ")":
            j += 1
        else:
            around = " ".join(n[max(0, j - 3) : j + 3])
            return f"base token {i} `{b[i] if i < len(b) else '<end>'}`, new token {j} `{n[j] if j < len(n) else '<end>'}` near `{around}`"
    return None


def _glue_texts(items: list[Item]) -> list[str]:
    return [" ".join(t.text for t in it.toks[it.lead : it.end]) for it in items if it.glue]


def _shown(item: Item) -> str:
    """The line as written, attributes included, without the comments above it."""
    first = next(k for k in range(item.lead, item.head + 1) if item.toks[k].kind not in ("com", "doc"))
    return " ".join(item.text[item.toks[first].start : item.stop].split())


def _declared_modules(read: Callable[[Path], str | None], mod_dir: Path, items: list[Item], prefix: str = "") -> list[tuple[str, Path, list[Item] | None]]:
    """(name, file, its items or None when the file is missing) of every module
    the `mod` lines of `items` declare, and of the modules those declare in turn:
    `group/part` for the part of a group. `read` gives a file's text or None."""
    out: list[tuple[str, Path, list[Item] | None]] = []
    for it in items:
        if it.kind != "mod_decl" or it.is_test_gated():
            continue
        path, name = mod_dir / f"{it.name}.rs", prefix + it.name
        body = read(path)
        held = None if body is None else cut_items(body)
        out.append((name, path, held))
        if held is not None:
            out += _declared_modules(read, mod_dir / it.name, held, name + "/")
    return out


def check(root: Path, parent_rel: str, base_text: str, base_ref: str | None = None) -> tuple[list[str], list[str]]:
    """(problems, report lines)."""
    problems: list[str] = []
    report: list[str] = []
    parent_path = root / parent_rel
    mod_dir = parent_path.with_suffix("")
    new_items = cut_items(parent_path.read_text())
    base_items = cut_items(base_text)

    # entries: (home, order key, item); home is "parent" or a module name
    base: list[tuple[str, tuple[int, int], Item]] = [
        ("parent", (1, i), it) for i, it in enumerate(base_items) if not it.glue
    ]
    if base_ref:

        def at_ref(path: Path) -> str | None:
            return _git_show(root, base_ref, str(path.relative_to(root)))

        for name, _, its in _declared_modules(at_ref, mod_dir, base_items):
            base += [(name, (0, i), x) for i, x in enumerate(its or []) if not x.glue]
    new: list[tuple[str, int, Item]] = [("parent", i, it) for i, it in enumerate(new_items) if not it.glue]
    module_glue: dict[str, list[Item]] = {}
    test_files = [mod_dir / f"{it.name}.rs" for it in new_items if it.kind == "mod_decl" and it.is_test_gated()]

    def on_disk(path: Path) -> str | None:
        return path.read_text() if path.is_file() else None

    for name, path, its in _declared_modules(on_disk, mod_dir, new_items):
        if its is None:
            problems.append(f"`mod {name.split('/')[-1]};` has no file {path.relative_to(root)}")
            continue
        module_glue[name] = [x for x in its if x.glue]
        new += [(name, i, x) for i, x in enumerate(its) if not x.glue]

    by_key: dict[str, list] = {}
    for entry in base:
        by_key.setdefault(entry[2].key, []).append(entry)
    new_by_key: dict[str, list] = {}
    for entry in new:
        new_by_key.setdefault(entry[2].key, []).append(entry)
    pairs: list[tuple[tuple, tuple]] = []
    for key in sorted(set(by_key) | set(new_by_key)):
        bl, nl = by_key.get(key, []), list(new_by_key.get(key, []))
        label = key.split(":", 1)[1]
        if not bl:
            problems.append(f"extra item `{label}` ({nl[0][0]}) is not in the base")
            continue
        if not nl:
            problems.append(f"item `{label}` is missing from the new tree")
            continue
        if len(nl) > len(bl):
            problems.append(f"item `{label}` is present {len(nl)} times (base: {len(bl)}): " + ", ".join(e[0] for e in nl))
        for b in bl:
            hit = next((e for e in nl if _align(b[2], e[2]) is None), None)
            if hit is None and not nl:
                problems.append(f"item `{label}` is missing from the new tree")
            elif hit is None:
                problems.append(f"item `{label}` differs from the base: {_align(b[2], nl[0][2])}")
            else:
                nl.remove(hit)
                pairs.append((b, hit))

    for home in sorted({n[0] for _, n in pairs}):
        seq = sorted(((n[1], b[1], n[2]) for b, n in pairs if n[0] == home), key=lambda x: x[0])
        for (_, k1, first), (_, k2, second) in zip(seq, seq[1:]):
            if k2 < k1:
                where = "the parent" if home == "parent" else f"module `{home}`"
                problems.append(f"order changed in {where}: `{second.display or second.name}` now stands after `{first.display or first.name}`")
    for b, n in pairs:
        if b[0] != "parent" and n[0] != b[0]:
            problems.append(f"`{n[2].display or n[2].name}` moved from `{b[0]}` to `{n[0]}`")

    base_tests = [it for it in base_items if _is_cfg_test_mod(it)]
    now_tests = [it for it in new_items if _is_cfg_test_mod(it)]
    if _glue_texts(base_tests) != _glue_texts(now_tests):
        problems.append("the test module declarations changed")
    for path in test_files:
        if base_ref:
            was = _git_show(root, base_ref, str(path.relative_to(root)))
            if was is None or was != path.read_text():
                problems.append(f"test file {path.relative_to(root)} differs from {base_ref}")
        elif not path.is_file():
            problems.append(f"test file {path.relative_to(root)} is missing")

    base_glue, now_glue = _glue_texts(base_items), _glue_texts(new_items)
    for line in base_glue:
        if line not in now_glue:
            problems.append(f"a `use`/`mod` line was removed: {line}")
    report.append("glue (use/mod lines) added to the parent:")
    added = [_shown(it) for it, line in zip([i for i in new_items if i.glue], now_glue) if line not in base_glue]
    report += [f"  + {line}" for line in added] or ["  (none)"]
    for mod, glue in module_glue.items():
        report += [f"glue in {mod}.rs: {_shown(g)}" for g in glue]
    homes: dict[str, int] = {}
    for b, n in pairs:
        homes[n[0]] = homes.get(n[0], 0) + 1
    moved = {h: c for h, c in homes.items() if h != "parent"}
    report.append(f"items in base: {len([b for b in base if b[0] == 'parent'])}")
    report.append("moved: " + (", ".join(f"{m} {c}" for m, c in sorted(moved.items())) or "none"))
    report.append(f"left in the parent: {homes.get('parent', 0)}")
    report.append(f"problems: {len(problems)}")
    return problems, report


# ----------------------------------------------------------------- map


def move_map(
    base_text: str, plan_path: Path, root: Path | None = None, parent_rel: str = DEFAULT_FILE, base_ref: str | None = None
) -> list[str]:
    """With `base_ref`, an item that the modules already held at that commit is
    an earlier PR's move and is not listed.

    Earlier moves are told apart item by item, by their tokens, and not by the
    plan's name or by how an item is shown: one name covers a type and its
    impls, a type may have several `impl` blocks that are shown alike, and any
    of them may move in a different PR."""
    modules, _, _, _ = load_plan(plan_path)
    items = cut_items(base_text)
    earlier: list[Item] = []
    if root is not None and base_ref:
        mod_dir = Path(parent_rel).with_suffix("")
        for mod in modules:
            body = _git_show(root, base_ref, f"{mod_dir}/{mod}.rs")
            if body is not None:
                earlier += [x for x in cut_items(body) if not x.glue]

    def moved_before(it: Item) -> bool:
        return any((x.display or x.name) == (it.display or it.name) and _align(it, x) is None for x in earlier)

    rows = []
    for mod, names in modules.items():
        for name in names:
            found = [it for it in items if _matches(it, name)]
            hits = [it for it in found if not moved_before(it)]
            if not found and not any(_matches(x, name) for x in earlier):
                raise SplitError(f"the plan names `{name}` and the base file has no such item")
            rows += [(it.start, f"{it.display or name}: {line_of(base_text, it.start)}-{line_of(base_text, it.stop)} -> {mod}") for it in hits]
    return [r for _, r in sorted(rows)]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", type=Path, default=Path(__file__).resolve().parent.parent, help="repository root")
    ap.add_argument("--file", default=DEFAULT_FILE, help=f"the file to cut, relative to the root (default {DEFAULT_FILE})")
    ap.add_argument("--plan", type=Path, default=DEFAULT_PLAN)
    ap.add_argument("--check", metavar="BASE_FILE", type=Path)
    ap.add_argument("--base-ref", metavar="REF", help="with --check: the commit whose modules and test files are the base; with --map: the commit whose moves are not listed")
    ap.add_argument("--map", nargs="?", const="", metavar="BASE_FILE")
    args = ap.parse_args()
    root = args.root.resolve()
    try:
        if args.check:
            problems, report = check(root, args.file, args.check.read_text(), args.base_ref)
            for p in problems:
                print(f"PROBLEM: {p}")
            print("\n".join(report))
            return 1 if problems else 0
        if args.map is not None:
            base = Path(args.map) if args.map else root / args.file
            print("\n".join(move_map(base.read_text(), args.plan, root, args.file, args.base_ref)))
            return 0
        written = apply(root, args.file, args.plan)
        print("\n".join(f"wrote {w}" for w in written) or "nothing to change")
        return 0
    except SplitError as e:
        print(f"error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
