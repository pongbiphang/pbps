#!/usr/bin/env python3
"""Collapse whole-family Change patterns (#2790 step 3a).

Usage: change_family_collapse.py ROOT [--check]

This is separate from the single-family mover because it needs all family
inventories at once and must remain idempotent after deleting the macros.
The mover supplies the lexer and scope resolver; inventories come from the
family enums, never from a second list of kinds. Only crates/**/*.rs is rewritten.
Unknown attribution aborts the entire write, with every site reported.
Comments between alternatives, guards, bindings and partial sets are retained.
Rule c also preserves arms whose leading comment says 'rather than a wildcard'.
"""

import argparse
import re
import sys
from collections import Counter, defaultdict
from pathlib import Path

import change_family_rewrite as cfr

FAMILIES = dict(zip(
    'tables table_properties partitions columns constraints indexes policies rows '
    'modules permissions type_values roles types search extensions'.split(),
    'Table TableProperty Partition Column Constraint Index Policy Row '
    'Module Permission TypeValue Role Type Search Extension'.split(),
))
CATALOG = ('roles', 'types', 'search', 'extensions')
MACROS = {'change_family', 'catalog_change'}

# The former pin read the macro's source. Once the macro is gone, keep its
# actual obligation: both classifiers explicitly decide every outer family.
CLASSIFIER_PIN = '''fn every_variant_is_listed_in_exactly_one_family() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/change.rs");
    let text = std::fs::read_to_string(path).unwrap();
    let file = syn::parse_file(&text).unwrap();
    let kinds = change_kinds_of_file::change_kinds(&file);
    let unique: std::collections::BTreeSet<_> = kinds.iter().collect();
    assert_eq!(kinds.len(), unique.len(), "a kind belongs to exactly one family");
    assert!(kinds.len() > 3);
    let change = file.items.iter().find_map(|item| {
        if let syn::Item::Enum(e) = item
            && e.ident == "Change"
        {
            Some(e)
        } else {
            None
        }
    }).unwrap();
    for classifier in ["pub fn catalog_object", "pub fn family"] {
        let start = text.find(classifier).unwrap();
        let body = &text[start..];
        let body = &body[..body.find("\\n    }\\n").unwrap()];
        assert!(
            !body.contains("_ =>"),
            "a wildcard arm in `{classifier}` hides new families"
        );
        for family in &change.variants {
            let pattern = format!("Change::{}(_)", family.ident);
            assert_eq!(
                body.matches(&pattern).count(), 1,
                "`{classifier}` must decide family `{}` exactly once", family.ident
            );
        }
    }
}'''


def enum_members(f, name):
    """Variant name, payload token range, and full token range, without attrs."""
    ts = f.toks
    for i in range(len(ts) - 2):
        if [t.text for t in ts[i:i + 3]] != ['enum', name, '{']:
            continue
        end, j = f.close_of[i + 2], i + 3
        members = []
        while j < end:
            while ts[j].text == '#':
                j = f.close_of[j + 1] + 1
            start = j
            j += 1
            payload = None
            if ts[j].text in ('(', '{'):
                payload = (j, f.close_of[j])
                j = payload[1] + 1
            if ts[j].text not in (',', '}'):
                raise ValueError(f'unsupported discriminant in enum {name}')
            members.append((ts[start].text, payload, (start, j)))
            if ts[j].text == ',':
                j += 1
        return members
    raise ValueError(f'enum {name} not found')


def apply_edits(src, edits):
    out, last = [], 0
    for start, end, text in sorted(edits):
        if start < last:
            raise ValueError('overlapping rewrites')
        out.extend((src[last:start], text))
        last = end
    out.append(src[last:])
    return ''.join(out)


class Collapse(cfr.Project):
    def __init__(self, root):
        super().__init__(root, '', '', '', [], {})
        self.counts = Counter()
        self.outputs = {}
        self.skipped_arms = []
        f = self.facts(root / cfr.CHANGE_FILE)
        self.inventory = {}
        by_outer = {name: payload for name, payload, _ in enum_members(f, 'Change')}
        for family, outer in FAMILIES.items():
            if outer not in by_outer:
                continue
            a, b = by_outer[outer]
            held = f.toks[a + 1:b]
            if len(held) != 1 or held[0].kind != 'ident':
                raise ValueError(f'{outer} is not a single family enum')
            inner = held[0].text
            kinds = {name for name, _, _ in enum_members(f, inner)}
            self.inventory[outer] = (inner, kinds)

    def edit(self, edits, start, end, text, rule, count=1):
        edits.append((start, end, text))
        self.counts[rule] += count

    def path_status(self, f, a, b):
        """Attribute a path ending in Change (or its in-scope alias)."""
        ts = f.toks
        segs = [t.text for t in ts[a:b] if t.text != '::']
        if len(segs) == 1:
            return self.resolve(f.path, f.scope_of[a], segs[0])[0]
        _, in_model, _ = self.crate_info(f.path)
        if segs[0] == 'pbps_model' or self.alias_status(f.path, f.scope_of[a], segs[0]) == 'model':
            return 'model' if segs[-1] == 'Change' else 'other'
        if segs[0] in ('crate', '$crate', 'super', 'self') and in_model and segs[-1] == 'Change':
            return 'model'
        return 'unknown'

    def macro_change_path(self, f, start, macro):
        ts = f.toks
        segs = [t.text for t in ts[start:macro] if t.text != '::']
        ctx = f.ctx_of[macro]
        if ctx and ctx[0] == 'macro_rules':
            if segs == ['$crate'] and self.crate_info(f.path)[1]:
                return '$crate::Change'
            return None  # A macro's expansion scope is not its definition scope.
        if ctx and ctx[0] == 'macro' and ctx[1] not in cfr.TRANSPARENT_MACROS:
            return None
        names = ['Change']
        for stmt in f.uses:
            names.extend(e.alias for e in stmt.entries if e.alias and e.segs[-1] == 'Change')
        names.extend(f.type_alias)
        for name in dict.fromkeys(names):
            if self.resolve(f.path, f.scope_of[macro], name)[0].startswith('model'):
                return name
        if segs and (segs[0] == 'pbps_model' or
                     self.alias_status(f.path, f.scope_of[macro], segs[0]) == 'model' or
                     (segs[0] in ('crate', '$crate') and self.crate_info(f.path)[1])):
            return f.src[ts[start].start:ts[macro].start] + 'Change'
        return None

    def definitions(self, f, edits):
        """Delete the exported macros with their documentation and attributes."""
        ts, ranges = f.toks, []
        for i in range(len(ts) - 3):
            if ts[i].text != 'macro_rules' or ts[i + 2].text not in MACROS:
                continue
            if str(f.path.relative_to(self.root)) != cfr.CHANGE_FILE:
                self.report(f, ts[i].start, 'family macro is redefined outside change.rs')
                continue
            a = i
            while a >= 2 and ts[a - 1].text == ']':
                op = next(o for o, c in f.close_of.items() if c == a - 1)
                if ts[op - 1].text != '#':
                    break
                a = op - 1
            start = f.src.rfind('\n', 0, ts[a].start) + 1
            while start:
                prev = f.src.rfind('\n', 0, start - 1) + 1
                if not f.src[prev:start].lstrip().startswith('///'):
                    break
                start = prev
            end = ts[f.close_of[i + 3]].end
            if f.src[end:end + 2] == '\n\n':
                end += 2
            ranges.append((start, end))
            self.edit(edits, start, end, '', 'd_macro_definitions')
        return ranges

    def macros(self, f, edits, excluded):
        ts = f.toks
        for i in range(len(ts) - 2):
            if ts[i].text not in MACROS or ts[i + 1].text != '!':
                continue
            if any(a <= ts[i].start < b for a, b in excluded):
                continue
            start = self.path_start(f, i)
            op = i + 2
            end = f.close_of.get(op)
            if end is None:
                self.report(f, ts[i].start, 'unrecognised family macro arguments')
                continue
            args = ts[op + 1:end]
            families = CATALOG if not args and ts[i].text == 'catalog_change' else (
                (args[0].text,) if len(args) == 1 else ())
            valid = CATALOG if ts[i].text == 'catalog_change' else FAMILIES
            path = self.macro_change_path(f, start, i)
            if not families or any(k not in valid or FAMILIES[k] not in self.inventory for k in families) or not path:
                self.report(f, ts[i].start, 'cannot attribute family macro or its Change path')
                continue
            if any(ts[j].end != ts[j + 1].start for j in range(start, i)) or any(
                ts[start].start <= c.start < ts[end].end for c in f.comments
            ):
                self.report(f, ts[i].start, 'comment or split path in family macro')
                continue
            text = ' | '.join(f'{path}::{FAMILIES[k]}(_)' for k in families)
            self.edit(edits, ts[start].start, ts[end].end, text,
                      'a_change_family' if ts[i].text == 'change_family' else 'b_catalog_change')

    def children(self, f, a, b):
        """Top-level token/group indices in a half-open token range."""
        while a < b:
            yield a
            a = f.close_of[a] + 1 if a in f.close_of else a + 1

    def leading_comment(self, f, a):
        """The comment block attached to a pattern, excluding trailing comments.

        An explicit reason to enumerate members is a decision about future
        kinds. It applies to the entire pattern, including nested wrappers.
        """
        offset = f.toks[a].start
        block = []
        for comment in reversed(f.comments):
            if comment.end > offset:
                continue
            if f.src[comment.end:offset].strip():
                break
            line_start = f.src.rfind('\n', 0, comment.start) + 1
            if f.src[line_start:comment.start].strip() and f.src.startswith('//', comment.start):
                previous = next((t for t in reversed(f.toks[:a]) if t.end <= comment.start), None)
                if previous is None or previous.text != '{':
                    break  # A previous arm's trailing comment is not this arm's reason.
            if block and f.line(offset) > f.line(comment.end) + 1:
                break
            block.append(comment)
            offset = comment.start
        if not block:
            return None
        start, end = block[-1].start, block[0].end
        return start, f.src[start:end]

    def preserves_member_listing(self, f, a):
        block = self.leading_comment(f, a)
        if block is None:
            return False
        # Ignore comment markers and wrapping so the same reason works in
        # line, block and documentation comments.
        words = re.sub(r'(?m)^\s*(?:///?!?|/\*+|\*+/?|//)', '', block[1])
        if 'rather than a wildcard' not in ' '.join(words.casefold().split()):
            return False
        self.skipped_arms.append((self.rel(f.path), f.line(f.toks[a].start), f.line(block[0]), block[1]))
        return True

    def pattern_ranges(self, f):
        ts = f.toks
        for i, t in enumerate(ts):
            ctx = f.ctx_of[i]
            if ctx and (ctx[0] == 'macro_rules' or ctx[1] not in cfr.TRANSPARENT_MACROS or ctx[1] in f.user_macros):
                continue
            if t.text in ('matches', 'assert_matches') and i + 2 < len(ts) and ts[i + 1].text == '!':
                op = i + 2
                if op not in f.close_of:
                    continue
                end = f.close_of[op]
                commas = [j for j in self.children(f, op + 1, end) if ts[j].text == ',']
                if commas:
                    yield commas[0] + 1, commas[1] if len(commas) > 1 else end
            if t.text != 'match':
                continue
            j = i + 1
            while j < len(ts) and ts[j].text not in ('{', ';', '=>'):
                j = f.close_of[j] + 1 if j in f.close_of else j + 1
            if j not in f.close_of or ts[j].text != '{':
                continue
            children = list(self.children(f, j + 1, f.close_of[j]))
            start = 0
            for k, idx in enumerate(children):
                if ts[idx].text != '=>':
                    continue
                between = children[start:k]
                commas = [n for n in range(start, k) if ts[children[n]].text == ',']
                if commas:
                    a = children[commas[-1] + 1]
                else:
                    a = between[0] if between else idx
                yield a, idx
                # A block arm need not end in a comma.
                start = k + 1
                if start < len(children) and ts[children[start]].text == '{':
                    start += 1

    def candidates(self, f, a, b):
        """Family members under wrappers, with a token-shaped hole key.

        Everything outside the hole must match, including tuple positions and
        path qualifiers. Bindings anywhere in a wrapper exclude it.
        """
        ts = f.toks
        found = []
        for i in range(a, b - 3):
            outer = ts[i].text
            if outer not in self.inventory or ts[i - 1].text != '::' or ts[i + 1].text != '(':
                continue
            end = f.close_of[i + 1]
            if end >= b:
                continue
            ps = self.path_start(f, i - 2)
            if ps < a:
                continue
            inner, kinds = self.inventory[outer]
            inside = ts[i + 2:end]
            # Rust permits a trailing comma inside the outer tuple variant.
            if inside and inside[-1].text == ',':
                inside = inside[:-1]
            kin = next((n for n in range(len(inside) - 2)
                        if inside[n].text == inner and inside[n + 1].text == '::'), None)
            if kin is None:
                continue
            kind = inside[kin + 2].text
            if kind not in kinds:
                continue
            payload = tuple(t.text for t in inside[kin + 3:])
            free = payload in (('{', '.', '.', '}'), ('(', '.', '.', ')'), ('(', '_', ')'), ())
            # The outer spelling determines the grouping. A differently
            # qualified inner enum needs its own attribution before collapsing.
            outer_prefix = tuple(t.text for t in ts[ps:i - 2])
            inner_prefix = tuple(t.text for t in inside[:kin])
            inner_status = 'model'
            if outer_prefix != inner_prefix:
                qualifier = next((s for s in inner_prefix if s != '::'), None)
                if qualifier:
                    in_model = self.crate_info(f.path)[1]
                    ours = qualifier == 'pbps_model' or self.alias_status(f.path, f.scope_of[i], qualifier) == 'model'
                    ours |= in_model and qualifier in ('crate', '$crate', 'self', 'super')
                    inner_status = 'model' if ours else 'unknown'
                else:
                    self.inner = inner
                    inner_status = 'model' if self.inner_status(f.path, f.scope_of[i], 'pbps_model') == 'present' else 'unknown'
            wrapper = tuple(t.text for t in ts[a:ps]) + ('<MEMBER>',) + tuple(t.text for t in ts[end + 1:b])
            # Restrict wrapper syntax to payload-free patterns. In particular a
            # variable in another tuple slot must not be erased with its arm.
            allowed = {'(', ')', '{', '}', '&', 'mut', ',', '_', '.', '::', ':'}
            outside = ts[a:ps] + ts[end + 1:b]
            for n, tok in enumerate(outside):
                if tok.text in allowed or tok.kind in ('num', 'str', 'char'):
                    continue
                before = outside[n - 1].text if n else ''
                after = outside[n + 1].text if n + 1 < len(outside) else ''
                if tok.kind == 'ident' and (after in ('(', '{', '::', ':') or before == '::'):
                    continue  # a constructor path
                free = False
            status = self.path_status(f, ps, i - 1)
            if status.startswith('model') and inner_status != 'model':
                status = inner_status
            path = f.src[ts[ps].start:ts[i].end]
            key = (outer, tuple(t.text for t in ts[ps:i]), wrapper)
            found.append((key, kind, free, status, ps, end + 1, path))
        return found

    def collapse_pattern(self, f, a, b, edits):
        ts = f.toks
        if a >= b or any(t.text in ('if', '@') for t in ts[a:b]):
            return
        if any(ts[a].start <= c.start < ts[b - 1].end for c in f.comments):
            return  # Keep the explanation attached to the alternatives it describes.
        bars = [j for j in self.children(f, a, b) if ts[j].text == '|']
        if bars:
            bounds = [a] + [j + 1 for j in bars]
            ends = bars + [b]
            groups = defaultdict(list)
            for n, (x, y) in enumerate(zip(bounds, ends)):
                if x == y:  # optional leading `|`
                    continue
                for c in self.candidates(f, x, y):
                    groups[c[0]].append((n, c))
            used = set()
            for key, members in groups.items():
                if any(n in used for n, _ in members):
                    continue
                outer = key[0]
                if {c[1] for _, c in members} != self.inventory[outer][1] or not all(c[2] for _, c in members):
                    continue
                if any(c[3] == 'unknown' for _, c in members):
                    for _, c in members:
                        if c[3] == 'unknown':
                            self.report(f, ts[c[4]].start, 'cannot attribute complete family pattern')
                    continue
                if not all(c[3].startswith('model') for _, c in members):
                    continue
                first, c = members[0]
                self.edit(edits, ts[c[4]].start, ts[c[5] - 1].end, c[6] + '(_)', 'c_complete_family')
                used.add(first)
                for n, _ in members[1:]:
                    # Delete the preceding bar and its whitespace, preserving
                    # the first member's location among other alternatives.
                    edits.append((ts[ends[n - 1] - 1].end, ts[ends[n] - 1].end, ''))
                    used.add(n)
            for n, (x, y) in enumerate(zip(bounds, ends)):
                if n not in used:
                    self.nested_patterns(f, x, y, edits)
        else:
            self.nested_patterns(f, a, b, edits)

    def nested_patterns(self, f, a, b, edits):
        ts = f.toks
        for j in self.children(f, a, b):
            if j not in f.close_of:
                continue
            end = f.close_of[j]
            # Each tuple/constructor argument is its own nesting position.
            commas = [k for k in self.children(f, j + 1, end) if ts[k].text == ',']
            bounds = [j + 1] + [k + 1 for k in commas]
            for x, y in zip(bounds, commas + [end]):
                self.collapse_pattern(f, x, y, edits)

    def prune_imports(self, path, src):
        """Remove only macro/family imports made unused by this rewrite."""
        toks, comments = cfr.lex(src)
        f = cfr.Facts(path, src, toks, comments)
        removable = MACROS | {'Change'} | {inner for inner, _ in self.inventory.values()}
        variants = {inner: kinds for inner, kinds in self.inventory.values()}

        def local_module(scope, segs):
            scope = self.module_scope(f, scope)
            for seg in segs:
                if seg == 'self':
                    continue
                if seg == 'super' and f.scopes[scope].parent is not None:
                    scope = self.module_scope(f, f.scopes[scope].parent)
                elif seg == 'crate' and path.name in ('lib.rs', 'main.rs'):
                    scope = 0
                else:
                    return None
            return scope

        def owners(scope, name, seen=frozenset()):
            """A child's independent import must not keep its parent's alive."""
            key = (scope, name)
            if key in seen:
                return set()
            seen = seen | {key}
            sc = f.scopes[scope]
            explicit = {(s.use_start, e) for s in sc.uses for e in s.entries
                        if not e.glob and (e.alias or e.segs[-1]) == name}
            if explicit or name in sc.decls:
                return explicit
            found = set()
            for s in sc.uses:
                for e in s.entries:
                    if not e.glob:
                        continue
                    if e.segs[-1] in variants and name in variants[e.segs[-1]]:
                        found.add((s.use_start, e))
                    else:
                        target = local_module(scope, e.segs)
                        if target is not None:
                            found.update(owners(target, name, seen))
            if found or sc.is_mod or sc.parent is None:
                return found
            return owners(sc.parent, name, seen)

        # Qualified paths use their first binding, not another import whose
        # name merely occurs later in the path. Relative module paths can
        # still reach an import in the parent of an inline module.
        used = set()
        imported = {e.alias or e.segs[-1] for s in f.uses for e in s.entries if not e.glob}
        imported.update(kind for kinds in variants.values() for kind in kinds)
        for i, t in enumerate(toks):
            if t.kind != 'ident' or t.text not in imported or any(s.use_start <= t.start < s.end for s in f.uses):
                continue
            scope = f.scope_of[i]
            ps = self.path_start(f, i)
            if ps < i:
                segs = [x.text for x in toks[ps:i] if x.text != '::']
                scope = local_module(scope, segs) if segs else None
            if scope is not None:
                used.update(owners(scope, t.text))
        # Public re-exports and used relative imports can depend on another
        # import. Preserve that supplier even when no expression names it.
        bindings = {(s.use_start, e): s for s in f.uses for e in s.entries}
        used.update(k for k, s in bindings.items() if s.vis.strip())
        pending = list(used)
        while pending:
            key = pending.pop()
            s, e = bindings[key], key[1]
            target = local_module(s.scope, e.segs[:-1]) if len(e.segs) > 1 else None
            if target is not None:
                for supplier in owners(target, e.segs[-1]) - used:
                    used.add(supplier)
                    pending.append(supplier)

        edits = []
        for s in f.uses:
            dead = [e for e in s.entries
                    if (any(seg in removable for seg in e.segs) or
                        (e.segs[-1] == 'change' and e.segs[0] in ('pbps_model', 'crate', 'super', 'self')))
                    and (s.use_start, e) not in used]
            if not dead:
                continue
            if len(dead) == len(s.entries):
                start, end = s.start, s.end
                if not src[s.line_start:start].strip() and src[end:end + 1] == '\n':
                    start, end = s.line_start, end + 1
                edits.append((start, end, ''))
            else:
                # Remove one whole use-tree leaf, including an alias and its
                # private qualifier; retain the surrounding shared use path.
                statement = src[s.use_start:s.end]
                for e in dead:
                    name = e.alias or e.segs[-1]
                    leaves, _ = cfr.lex(statement)
                    indices = [i for i, t in enumerate(leaves) if t.text == name]
                    if len(indices) != 1 or e.glob:
                        self.report(f, s.start, 'cannot remove grouped family import safely')
                        continue
                    i = indices[0]
                    start_i, end_i = i - 2 if e.alias else i, i
                    close_of = cfr.match_brackets(leaves)
                    while True:
                        while start_i >= 2 and leaves[start_i - 1].text == '::' and leaves[start_i - 2].kind == 'ident':
                            start_i -= 2
                        sole = (leaves[end_i + 1].text == '}' or
                                (leaves[end_i + 1].text == ',' and leaves[end_i + 2].text == '}'))
                        if leaves[start_i - 1].text == '{' and sole:
                            # Removing the sole child removes its whole subtree,
                            # rather than leaving an empty or unbalanced group.
                            op = start_i - 1
                            end_i = close_of[op]
                            start_i = op - 2
                        else:
                            break
                    if leaves[start_i - 1].text not in ('{', ','):
                        self.report(f, s.start, 'cannot remove import leaf safely')
                        continue
                    if leaves[end_i + 1].text == ',':
                        start, end = leaves[start_i].start, leaves[end_i + 2].start
                    else:
                        start, end = leaves[start_i - 1].start, leaves[end_i].end
                    statement = statement[:start] + statement[end:]
                edits.append((s.use_start, s.end, statement))
        return apply_edits(src, edits)

    def scan_file(self, path):
        f = self.facts(path)
        if isinstance(f, cfr.LexError):
            self.reports.append((self.rel(path), 1, f'cannot lex Rust file: {f}', ''))
            return
        edits = []
        excluded = self.definitions(f, edits)
        self.macros(f, edits, excluded)
        # Pattern collapse is done after macro replacement: this also avoids
        # treating a removed macro definition as executable Rust.
        new = apply_edits(f.src, edits)
        toks, comments = cfr.lex(new)
        g = cfr.Facts(path, new, toks, comments)
        self._facts[path] = g
        edits = []
        for a, b in self.pattern_ranges(g):
            if a < b and not self.preserves_member_listing(g, a):
                self.collapse_pattern(g, a, b, edits)
        new = apply_edits(new, edits)
        if self.rel(path) == cfr.CHANGE_FILE:
            # Deleted macros must not leave their public docs pointing at
            # missing items or claiming that each new inner kind is exhaustive.
            new = new.replace('/// The families of [`Change`] kinds; see [`change_family!`].',
                              '/// The families of [`Change`] kinds; see [`Change::family`].')
            for wording in ('`change_family!` in pattern position', 'an outer-family pattern'):
                new = new.replace(
                    f"    /// A consumer's arm: {wording}, in a match that\n"
                    "    /// stays exhaustive without a wildcard.",
                    "    /// A consumer's arm: whole-family patterns, in a match that stays\n"
                    "    /// exhaustive without a wildcard.")
            new = new.replace(
                "    /// Total on purpose, like [`Change::catalog_object`]: every variant is\n"
                "    /// reached through its family's patterns, so a new one does not compile\n"
                "    /// until it is added to an arm of [`change_family!`], and a variant listed\n"
                "    /// in two families is an unreachable pattern.",
                "    /// Total on families, like [`Change::catalog_object`]: a new family needs\n"
                "    /// an explicit arm. A new kind joins its family's existing decision.")
            new = new.replace(
                '    /// Total on purpose: each variant is named, so a new one does not compile\n'
                '    /// until someone decides which side it is on. Read this before listing a\n',
                '    /// Total on families: a new family needs an explicit decision; a new kind\n'
                "    /// joins its family's decision. Read this before listing a\n")
            # A pattern matches values; it does not name an enum. Replace the
            # former macro description as a whole before rewriting examples.
            for family, outer in FAMILIES.items():
                for pattern in (f'change_family!({family})', f'Change::{outer}(_)'):
                    new = new.replace(
                        f'/// The kinds of change of the `{family}` family (DEC-2790.1), the enum\n'
                        f'/// `{pattern}` names. [`Change::{outer}`] holds one. It is written\n',
                        f'/// The kinds of change of the `{family}` family (DEC-2790.1).\n'
                        f'/// [`Change::{outer}`] holds one. It is written\n')
        if self.rel(path) == 'crates/pbps-model/tests/catalog_object.rs' and 'macro_rules! change_family' in new:
            ts, _ = cfr.lex(new)
            close_of = cfr.match_brackets(ts)
            for i, t in enumerate(ts):
                if t.text == 'fn' and ts[i + 1].text == 'every_variant_is_listed_in_exactly_one_family':
                    op = next(j for j in range(i + 2, len(ts)) if ts[j].text == '{')
                    new = new[:t.start] + CLASSIFIER_PIN + new[ts[close_of[op]].end:]
                    self.counts['d_classifier_pin'] += 1
                    break
        # Documentation is safe to update as tokens of its own. String
        # literals remain byte-for-byte unchanged, including macro examples.
        _, comments = cfr.lex(new)
        doc_edits = []
        for comment in comments:
            old = new[comment.start:comment.end]
            replacement = re.sub(
                r'(?:\[`)?(?:crate::|pbps_model::)?change_family!\((\w+)\)(?:`\])?',
                lambda m: 'Change::' + FAMILIES[m[1]] + '(_)' if m[1] in FAMILIES else m[0], old)
            replacement = replacement.replace('[`catalog_change!`]', 'catalog family patterns')
            replacement = re.sub(r'catalog_change!\((\w*)\)',
                                 lambda m: ' | '.join('Change::' + FAMILIES[k] + '(_)'
                                                     for k in ((m[1],) if m[1] else CATALOG))
                                 if not m[1] or m[1] in CATALOG else m[0], replacement)
            replacement = replacement.replace('`catalog_change!`', 'catalog family patterns')
            replacement = replacement.replace(
                'Roles, types and extensions (catalog family patterns).',
                'Roles, types, search objects and extensions (catalog family patterns).')
            replacement = replacement.replace('`change_family!` in pattern position',
                                              'an outer-family pattern')
            if replacement != old:
                doc_edits.append((comment.start, comment.end, replacement))
        new = apply_edits(new, doc_edits)
        new = self.prune_imports(path, new)
        self.outputs[path] = new


def run(root, write=True, out=sys.stdout):
    project = Collapse(root.resolve())
    for path in cfr.rust_files(project.root / 'crates'):
        project.scan_file(path)
    changed = {p: s for p, s in project.outputs.items() if s != p.read_text()}
    removed = sum(p.read_text().count('\n') - s.count('\n') for p, s in changed.items())
    for rel, line, reason, text in sorted(set(project.reports)):
        print(f'{rel}:{line}: REPORT {reason}: {text}', file=out)
    for rel, line, comment_line, comment in sorted(set(project.skipped_arms)):
        print(f'{rel}:{line}: SKIP rule c: leading comment at line {comment_line}: {comment}', file=out)
    for rule, count in sorted(project.counts.items()):
        print(f'{rule}: {count} site(s)', file=out)
    print(f'{len(changed)} files; {removed} lines removed; {len(set(project.reports))} refused sites', file=out)
    if write and not project.reports:
        cfr.write_all(changed)
    elif project.reports:
        print('Nothing written: attribution reports must all be resolved.', file=out)
    project.changed, project.lines_removed = changed, removed
    return project


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('root', type=Path)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args(argv)
    return 1 if run(args.root, not args.check).reports else 0


if __name__ == '__main__':
    sys.exit(main())
