#!/usr/bin/env python3
"""Split a function's top-level `match change` into family modules (#2790).

Usage:
    change_family_split.py ROOT --file PATH --function NAME
    change_family_split.py ROOT --file PATH --function NAME --check --original FILE

The original file for --check must be saved before the split. Verification
checks the parent outside the match byte for byte, ignoring only the added mod
lines, and independently parses every moved arm. Arm tokens and comments must
remain identical, in order; whitespace may change when the primary runs fmt.
Multi-family or-patterns are projected once per named family, with identical
guards and bodies. Only their first family's copy carries the leading comment.
No build, formatter, git or network command is run by this tool.
"""

import argparse
import re
import sys
from collections import defaultdict
from dataclasses import dataclass
from pathlib import Path

import change_family_collapse as cfc
import change_family_rewrite as cfr


class Source(cfr.Facts):
    def __init__(self, path, text):
        tokens, comments = cfr.lex(text)
        super().__init__(path, text, tokens, comments)

    def children(self, start, end):
        return list(cfc.Collapse.children(None, self, start, end))

    def text(self, start, end):
        return self.src[self.toks[start].start:self.toks[end - 1].end]

    def refuse(self, token, reason):
        raise ValueError(f'{self.path}:{self.line(self.toks[token].start)}: {reason}')


def snake(name):
    return re.sub(r'(?<!^)(?=[A-Z][a-z]|(?<=[a-z0-9])[A-Z])', '_', name).lower()


def rust_name(outer):
    name = snake(outer)
    # `Type` is a model family, while `type` is a Rust keyword. The file
    # keeps its ordinary snake_case name; the Rust paths need raw identifiers.
    keywords = set('as break const continue crate else enum extern false fn for if impl in '
                   'let loop match mod move mut pub ref return self Self static struct super '
                   'trait true type unsafe use where while async await dyn abstract become '
                   'box do final macro override priv typeof unsized virtual yield try gen'.split())
    return 'r#' + name if name in keywords else name


def fingerprint(text):
    """Keep literal and comment bytes; only Rust whitespace is insignificant."""
    tokens, comments = cfr.lex(text)
    return [(kind, value) for _, kind, value in sorted(
        [(t.start, t.kind, t.text) for t in tokens]
        + [(c.start, 'comment', text[c.start:c.end]) for c in comments])]


@dataclass
class Parameter:
    name: str
    declaration: str
    type_text: str


@dataclass
class Function:
    parameters: list
    result: str
    arguments: int
    end_arguments: int
    body: int
    match: int
    opening: int
    closing: int


def function(source, name, binding='change'):
    ts = source.toks
    found = [i for i in source.children(0, len(ts))
             if ts[i].text == 'fn' and ts[i + 1].text == name]
    if len(found) != 1:
        raise ValueError(f'{source.path}: cannot resolve exactly one top-level function {name}')
    start = found[0]
    args = start + 2
    if ts[args].text != '(':
        source.refuse(args, 'generic function signatures are not supported')
    end_args = source.close_of[args]
    parameters, first, angle = [], args + 1, 0
    # The reused bracket walker skips tuples and callbacks; angle depth keeps
    # commas in generic parameter types out of the parameter separators.
    for i in source.children(first, end_args) + [end_args]:
        angle += (ts[i].text == '<') - (ts[i].text == '>')
        if i != end_args and (ts[i].text != ',' or angle):
            continue
        if first < i:
            if ts[first].kind != 'ident' or ts[first + 1].text != ':':
                source.refuse(first, 'only named, unmodified function parameters are supported')
            parameters.append(Parameter(ts[first].text, source.text(first, i),
                                        source.text(first + 2, i)))
        first = i + 1
    body = end_args + 1
    while body < len(ts) and ts[body].text not in ('{', ';'):
        body += 1
    if body not in source.close_of or ts[body].text != '{':
        source.refuse(start, 'function has no body')
    result = source.src[ts[end_args].end:ts[body].start].strip()
    if not result.startswith('->') or any(t.text == 'where' for t in ts[end_args + 1:body]):
        source.refuse(start, 'function must have a concrete return type')
    candidates = [i for i in source.children(body + 1, source.close_of[body])
                  if [t.text for t in ts[i:i + 3]] == ['match', binding, '{']]
    if len(candidates) != 1:
        source.refuse(start, f'cannot resolve exactly one top-level match {binding}')
    match = candidates[0]
    return Function(parameters, result, args, end_args, body, match, match + 2,
                    source.close_of[match + 2])


def body_fingerprint(text):
    """A body's fingerprint with rustfmt's elided arm braces undone.

    rustfmt rewrites `X => { expr }` as `X => expr,` when the block holds one
    expression and no comment, so `--check` after `cargo fmt` sees the same
    arm without its braces. Strip one such outer block on both sides: a block
    with a top-level `;` or a comment is a different spelling, and rustfmt
    keeps its braces, so it still has to match exactly.
    """
    fp = fingerprint(text)
    if len(fp) < 2 or fp[0][1] != '{' or fp[-1][1] != '}':
        return fp
    depth = 0
    for i, (kind, value) in enumerate(fp):
        if kind == 'comment':
            return fp
        if value in ('{', '(', '['):
            depth += 1
        elif value in ('}', ')', ']'):
            depth -= 1
            if depth == 0 and i != len(fp) - 1:
                return fp
        elif value == ';' and depth == 1:
            return fp
    return fp[1:-1]


@dataclass
class Arm:
    first: int
    pattern: str
    guard: str
    body: str
    comma: str
    leading: str
    trailing: str
    # Token ranges of the pattern and guard, excluding the arrow.
    pattern_end: int
    arrow: int


def arms(source, target):
    ts, result = source.toks, []
    first, end = target.opening + 1, target.closing
    previous_end = ts[target.opening].end
    while first < end:
        children = source.children(first, end)
        arrow = next((i for i in children if ts[i].text == '=>'), None)
        if arrow is None:
            source.refuse(first, 'cannot resolve arm arrow')
        guard = next((i for i in children if i < arrow and ts[i].text == 'if'), arrow)
        body_start = arrow + 1
        if body_start >= end:
            source.refuse(arrow, 'arm has no body')
        if ts[body_start].text == '{':
            body_end = source.close_of[body_start] + 1
            # A block followed by a postfix operator is an expression and
            # still needs its comma; a bare block may omit that comma.
            postfix = body_end < end and ts[body_end].text in ('.', '?')
        else:
            postfix = True
        if postfix:
            rest = source.children(body_start, end)
            stop = next((i for i in rest if ts[i].text in (',', '=>')), end)
            if stop < end and ts[stop].text == '=>':
                source.refuse(body_start, 'unsupported block expression without an arm comma')
            body_end = stop
        if body_end == body_start:
            source.refuse(body_start, 'empty arm body')
        after = body_end + (body_end < end and ts[body_end].text == ',')
        syntax_end = ts[after - 1].end

        # Attach only immediately preceding comment lines. A previous arm's
        # same-line comment belongs to that arm instead of the following one.
        attached = ts[first].start
        for comment in reversed(source.comments):
            if comment.end > attached or comment.start < previous_end:
                continue
            gap = source.src[comment.end:attached]
            line = source.src.rfind('\n', 0, comment.start) + 1
            if gap.strip() or gap.count('\n') > 1 or source.src[line:comment.start].strip():
                break
            attached = comment.start
        gap = source.src[previous_end:attached]
        if gap.strip():
            source.refuse(first, 'unattached comments between arms cannot be moved safely')
        leading = source.src[attached:ts[first].start] if attached < ts[first].start else ''

        trailing_end = syntax_end
        for comment in source.comments:
            if comment.start < syntax_end:
                continue
            gap = source.src[trailing_end:comment.start]
            if gap.strip() or '\n' in gap:
                break
            trailing_end = comment.end
        result.append(Arm(first, source.text(first, guard),
                          source.src[ts[guard - 1].end:ts[arrow].start],
                          source.src[ts[arrow].end:ts[body_end - 1].end],
                          ',' if after > body_end else '', leading,
                          source.src[syntax_end:trailing_end], guard, arrow))
        previous_end, first = trailing_end, after
    if source.src[previous_end:ts[end].start].strip():
        source.refuse(end, 'unattached trailing comments cannot be moved safely')
    return result


def inventory(root):
    path = root / cfr.CHANGE_FILE
    source = Source(path, path.read_text())
    types = {}
    for outer, payload, _ in cfc.enum_members(source, 'Change'):
        if payload is None:
            raise ValueError(f'{path}: {outer} has no inner family enum')
        a, b = payload
        held = source.toks[a + 1:b]
        if len(held) != 1 or held[0].kind != 'ident':
            source.refuse(a, f'{outer} is not a single family enum')
        types[outer] = held[0].text
    order = []
    for family, _, _ in cfc.enum_members(source, 'ChangeFamily'):
        outer = cfc.FAMILIES.get(snake(family))
        if outer not in types:
            raise ValueError(f'{path}: cannot associate ChangeFamily::{family} with Change')
        order.append(outer)
    if set(order) != set(types) or len(order) != len(types):
        raise ValueError(f'{path}: ChangeFamily must list every Change family exactly once')
    return types, order


def alternatives(source, arm, types):
    """Unwrap root alternatives, retaining each inner pattern's original text."""
    ts = source.toks
    bars = [i for i in source.children(arm.first, arm.pattern_end) if ts[i].text == '|']
    starts = [arm.first] + [i + 1 for i in bars]
    ends = bars + [arm.pattern_end]
    result = []
    for a, b in zip(starts, ends):
        if a == b:  # Rust permits a leading bar.
            continue
        if (b - a < 5 or [t.text for t in ts[a:a + 2]] != ['Change', '::']
                or ts[a + 2].text not in types or ts[a + 3].text != '('
                or source.close_of.get(a + 3) != b - 1):
            source.refuse(a, 'alternative is not a direct Change::<Family>(...) pattern')
        inner_end = b - 1
        if ts[inner_end - 1].text == ',':
            inner_end -= 1
        if a + 4 == inner_end:
            source.refuse(a, 'empty family payload pattern')
        lo, hi = ts[a + 4].start, ts[inner_end - 1].end
        # Comments in the inner pattern move with it. Erased wrappers and
        # separators must not hide an explanation that would otherwise vanish.
        if any(ts[a].start <= c.start < ts[b - 1].end and not lo <= c.start < hi
               for c in source.comments):
            source.refuse(a, 'comment in an erased outer wrapper')
        result.append((ts[a + 2].text, source.src[lo:hi]))
    spans = [(ts[a].start, ts[b - 1].end) for a, b in zip(starts, ends) if a < b]
    if any(ts[arm.first].start <= c.start < ts[arm.pattern_end - 1].end
           and not any(a <= c.start < b for a, b in spans) for c in source.comments):
        source.refuse(arm.first, 'comment between family alternatives has no unique owner')
    return result


def identifiers(text):
    return {t.text for t in cfr.lex(text)[0] if t.kind == 'ident'}


def bindings(pattern):
    """Conservative binding diagnostics; never repair a copied pattern."""
    ts = cfr.lex(pattern)[0]
    return {t.text for i, t in enumerate(ts) if t.kind == 'ident'
            and t.text not in ('ref', 'mut', '_')
            and (t.text[0].islower() or t.text.startswith('r#'))
            and (i == 0 or ts[i - 1].text != '::')
            and (i + 1 == len(ts) or ts[i + 1].text not in ('::', ':', '(', '{'))}


class Split:
    def __init__(self, root, file, name, original):
        self.root, self.file, self.name = root, file, name
        self.path = root / file
        self.source = source = Source(self.path, original)
        self.target = target = function(source, name)
        self.types, order = inventory(root)
        matched = [p for p in target.parameters if p.name == 'change']
        if len(matched) != 1 or fingerprint(matched[0].type_text) != fingerprint('&Change'):
            raise ValueError(f'{self.path}: matched parameter must be change: &Change')
        self.by_family = defaultdict(list)
        self.shared, self.reports = [], []
        for arm in arms(source, target):
            alts = alternatives(source, arm, self.types)
            families = list(dict.fromkeys(outer for outer, _ in alts))
            if len(families) > 1:
                self.shared.append((source.line(source.toks[arm.first].start), families))
            all_bindings = set().union(*(bindings(pat) for _, pat in alts))
            referenced = identifiers(arm.guard + ' ' + arm.body)
            for outer in families:
                pattern = ' | '.join(pat for family, pat in alts if family == outer)
                leading = arm.leading if outer == families[0] else ''
                self.by_family[outer].append((arm, pattern, leading))
                bound = bindings(pattern)
                for missing in sorted((all_bindings - bound) & referenced):
                    self.reports.append(f'{self.path}:{source.line(source.toks[arm.first].start)}: '
                                        f'{outer}: {missing} is bound only in another family; '
                                        'guard/body copied unchanged')
                for unused in sorted(bound - referenced):
                    if not unused.startswith('_'):
                        self.reports.append(f'{self.path}:{source.line(source.toks[arm.first].start)}: '
                                            f'{outer}: possibly unused binding {unused}; copied unchanged')
        self.families = [outer for outer in order if outer in self.by_family]
        # A family answered by one unguarded whole-family arm (`Change::Policy(_)
        # => refuse`) stays in the dispatch as it was: a module would hold
        # `match policy { _ => .. }`, which clippy rejects, around one line.
        self.inline = {outer for outer in self.families
                       if len(self.by_family[outer]) == 1
                       and fingerprint(self.by_family[outer][0][1]) == fingerprint('_')
                       and not self.by_family[outer][0][0].guard.strip()}
        self.modules = [outer for outer in self.families if outer not in self.inline]
        self.parameters = {}
        for outer in self.families:
            used = set().union(*(identifiers(arm.guard + ' ' + arm.body)
                                 for arm, _, _ in self.by_family[outer]))
            params = []
            local = rust_name(outer)
            if any(p.name == local for p in target.parameters):
                raise ValueError(f'{self.path}: family binding {local} clashes with a source parameter')
            for p in target.parameters:
                if p.name in used:
                    params.append(p)
                if p.name == 'change':
                    params.append(Parameter(local, f'{local}: &{self.types[outer]}',
                                            f'&{self.types[outer]}'))
            self.parameters[outer] = params

    def module_path(self, outer):
        return self.path.with_suffix('') / (snake(outer) + '.rs')

    def module(self, outer):
        local = rust_name(outer)
        header = (f'//! Emits the {snake(outer)} family of changes (DEC-2790.4).\n\n'
                  '// The moved arms use the parent emitter\'s private helpers, which stay there.\n'
                  'use super::*;\n\n'
                  'pub(super) fn emit(\n'
                  + ''.join(f'    {p.declaration},\n' for p in self.parameters[outer])
                  + f') {self.target.result} {{\n    match {local} {{\n')
        chunks = []
        for arm, pattern, leading in self.by_family[outer]:
            chunks.append(('        ' + leading if leading else '        ')
                          + pattern + arm.guard + '=>' + arm.body + arm.comma + arm.trailing + '\n')
        return header + ''.join(chunks) + '    }\n}\n'

    def dispatch(self):
        source, target = self.source, self.target
        indent_start = source.src.rfind('\n', 0, source.toks[target.match].start) + 1
        indent = source.src[indent_start:source.toks[target.match].start]
        if indent.strip():
            raise ValueError(f'{self.path}: target match must start on its own line')
        lines = ['match change {\n']
        for outer in self.families:
            if outer in self.inline:
                arm, _, leading = self.by_family[outer][0]
                lines.append(f'{indent}    {leading}Change::{outer}(_) =>{arm.body}'
                             f'{arm.comma or ","}{arm.trailing}\n')
                continue
            local = rust_name(outer)
            args = ', '.join(p.name for p in self.parameters[outer])
            lines.append(f'{indent}    Change::{outer}({local}) => {local}::emit({args}),\n')
        return ''.join(lines) + indent + '}'

    def declarations(self):
        source, ts = self.source, self.source.toks
        mods = [(ts[i + 1].text, i) for i in source.children(0, len(ts)) if ts[i].text == 'mod']
        added = {rust_name(outer) for outer in self.modules}
        if added & {name for name, _ in mods}:
            raise ValueError(f'{self.path}: family module clashes with an existing module')
        if any(ts[i + 2].text != ';' for _, i in mods):
            # Inline test modules are irrelevant; retain only out-of-line mods.
            mods = [(name, i) for name, i in mods if ts[i + 2].text == ';']
        if not mods:
            # A parent with no out-of-line module yet (SQL Server's emitter)
            # takes the new ones on the lines after its last top-level `use`,
            # where a reader looks for a file's structure. Without such a use
            # there is no settled place, so refuse rather than guess.
            top = source.children(0, len(ts))
            uses = [i for i in top if ts[i].text == 'use']
            if not uses:
                raise ValueError(f'{self.path}: no module declaration or top-level use to place new modules beside')
            end = next((j for j in top if j > uses[-1] and ts[j].text == ';'), None)
            if end is None:
                source.refuse(uses[-1], 'cannot find the end of the last use')
            offset = ts[end].end
            if source.src[offset:offset + 1] != '\n':
                source.refuse(uses[-1], 'the last use must end its line')
            text = ''.join(f'mod {name};\n' for name in sorted(added, key=lambda value: value.removeprefix('r#')))
            return [(offset + 1, offset + 1, text)]
        edits = defaultdict(str)
        for name in sorted(added, key=lambda value: value.removeprefix('r#')):
            later = next((i for existing, i in mods if existing.removeprefix('r#') > name.removeprefix('r#')), None)
            if later is None:
                i = mods[-1][1]
                offset = ts[i + 2].end
                if source.src[offset:offset + 1] != '\n':
                    source.refuse(i, 'module declaration must occupy its own line')
                offset += 1
            else:
                offset = source.src.rfind('\n', 0, ts[later].start) + 1
                # Include visibility preceding `mod` on the declaration line.
            edits[offset] += f'mod {name};\n'
        return [(offset, offset, text) for offset, text in edits.items()]

    def outputs(self):
        source, target = self.source, self.target
        edits = self.declarations()
        edits.append((source.toks[target.match].start, source.toks[target.closing].end,
                      self.dispatch()))
        return {self.path: cfc.apply_edits(source.src, edits),
                **{self.module_path(outer): self.module(outer) for outer in self.modules}}

    def verify(self, texts, verbatim=False):
        """Parse the result, checking multiplicity, ordering, and every boundary."""
        parent = Source(self.path, texts[self.path])
        target = function(parent, self.name)
        actual = arms(parent, target)
        if len(actual) != len(self.families):
            raise ValueError('verification: dispatch arm count differs')
        for arm, outer in zip(actual, self.families):
            if outer in self.inline:
                original, _, leading = self.by_family[outer][0]
                if (fingerprint(arm.pattern) != fingerprint(f'Change::{outer}(_)')
                        or arm.guard.strip() or fingerprint(arm.leading) != fingerprint(leading)
                        or fingerprint(arm.trailing) != fingerprint(original.trailing)
                        or body_fingerprint(arm.body) != body_fingerprint(original.body)):
                    raise ValueError(f'verification: incorrect dispatch for {outer}')
                continue
            local = rust_name(outer)
            args = ', '.join(p.name for p in self.parameters[outer])
            if (fingerprint(arm.pattern) != fingerprint(f'Change::{outer}({local})')
                    or arm.guard.strip() or arm.leading or arm.trailing
                    or fingerprint(arm.body) != fingerprint(f'{local}::emit({args})')):
                raise ValueError(f'verification: incorrect dispatch for {outer}')

        def outside(source, fn):
            return (source.src[:source.toks[fn.match].start] + '<MATCH>'
                    + source.src[source.toks[fn.closing].end:])

        edits = []
        for outer in self.modules:
            name = rust_name(outer)
            mods = [i for i in parent.children(0, len(parent.toks))
                    if [t.text for t in parent.toks[i:i + 3]] == ['mod', name, ';']]
            if len(mods) != 1:
                raise ValueError(f'verification: expected exactly one mod {name}')
            i = mods[0]
            start = parent.src.rfind('\n', 0, parent.toks[i].start) + 1
            end = parent.toks[i + 2].end
            if parent.src[start:end + 1] != f'mod {name};\n':
                raise ValueError(f'verification: unexpected module declaration for {name}')
            edits.append((start, end + 1, ''))
        edits.append((parent.toks[target.match].start, parent.toks[target.closing].end, '<MATCH>'))
        if cfc.apply_edits(parent.src, edits) != outside(self.source, self.target):
            raise ValueError('verification: parent changed outside the match and new mod lines')

        for outer in self.modules:
            path = self.module_path(outer)
            moved = Source(path, texts[path])
            fn = function(moved, 'emit', rust_name(outer))
            expected = Source(path, self.module(outer))
            expected_fn = function(expected, 'emit', rust_name(outer))

            def skeleton(source, target):
                # Formatting a short signature onto one line removes its
                # optional final comma. Compare each parameter separately so
                # that this cannot hide an added or reordered parameter.
                return cfc.apply_edits(source.src, [
                    (source.toks[target.arguments].start,
                     source.toks[target.end_arguments].end, '<PARAMETERS>'),
                    (source.toks[target.match].start,
                     source.toks[target.closing].end, '<MATCH>')])

            def parameter_comments(source, target):
                start = source.toks[target.arguments].start
                end = source.toks[target.end_arguments].end
                return [source.src[c.start:c.end] for c in source.comments if start <= c.start < end]

            parameter_pairs = zip(fn.parameters, expected_fn.parameters)
            if (len(fn.parameters) != len(expected_fn.parameters)
                    or any(fingerprint(a.declaration) != fingerprint(b.declaration)
                           for a, b in parameter_pairs)
                    or parameter_comments(moved, fn) != parameter_comments(expected, expected_fn)
                    or fingerprint(skeleton(moved, fn)) != fingerprint(skeleton(expected, expected_fn))):
                raise ValueError(f'verification: unexpected code or parameters in {path}')
            copied = arms(moved, fn)
            originals = self.by_family[outer]
            if len(copied) != len(originals):
                raise ValueError(f'verification: arm multiplicity differs in {path}')
            for actual, (original, pattern, leading) in zip(copied, originals):
                pairs = [(actual.pattern, pattern), (actual.guard, original.guard),
                         (actual.leading, leading), (actual.trailing, original.trailing)]
                if (any(fingerprint(a) != fingerprint(b) for a, b in pairs)
                        or body_fingerprint(actual.body) != body_fingerprint(original.body)):
                    raise ValueError(f'verification: arm content or order differs in {path}')
                if verbatim and actual.body != original.body:
                    raise ValueError(f'verification: body was not moved verbatim in {path}')

    def report(self, out):
        for outer in self.families:
            if outer in self.inline:
                print(f'{outer}: one whole-family arm, kept in the dispatch', file=out)
                continue
            print(f'{outer}: {len(self.by_family[outer])} arms; '
                  f'parameters: {", ".join(p.name for p in self.parameters[outer])}', file=out)
        for line, families in self.shared:
            print(f'SPLIT {self.file}:{line}: {", ".join(families)}', file=out)
        for report in self.reports:
            print(f'REPORT {report}', file=out)


def run(root, file, name, original=None, check=False, out=sys.stdout):
    root = Path(root).resolve()
    path = (root / file).resolve()
    if not path.is_relative_to(root) or path.suffix != '.rs':
        raise ValueError('target must be a Rust file within the root')
    if check and original is None:
        raise ValueError('--check requires --original with the pre-split source')
    split = Split(root, path.relative_to(root), name, path.read_text() if original is None else original)
    if check:
        split.verify({p: p.read_text() for p in [split.path]
                      + [split.module_path(outer) for outer in split.modules]})
    else:
        outputs = split.outputs()
        for outer in split.modules:
            p = split.module_path(outer)
            if not p.resolve().is_relative_to(root):
                raise ValueError(f'family output escapes the root: {p}')
            if p.exists() or p.with_suffix('').exists():
                raise ValueError(f'family output clashes with existing path: {p}')
        split.verify(outputs, verbatim=True)
        old = {p: p.read_bytes() if p.exists() else None for p in outputs}
        created_dirs = []
        try:
            # Validate every output before any write and restore even newly
            # created files on a failed write or post-write verification.
            for p, text in outputs.items():
                if not p.parent.exists():
                    p.parent.mkdir()
                    created_dirs.append(p.parent)
                p.write_text(text)
            split.verify({p: p.read_text() for p in outputs}, verbatim=True)
        except BaseException:
            for p, data in old.items():
                if data is None:
                    p.unlink(missing_ok=True)
                else:
                    p.write_bytes(data)
            for directory in reversed(created_dirs):
                directory.rmdir()
            raise
    split.report(out)
    return split


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('root', type=Path)
    parser.add_argument('--file', required=True)
    parser.add_argument('--function', required=True)
    parser.add_argument('--check', action='store_true')
    parser.add_argument('--original', type=Path)
    args = parser.parse_args(argv)
    if args.check and args.original is None:
        parser.error('--check requires --original with the pre-split source')
    if args.original is not None and not args.check:
        parser.error('--original is only used with --check')
    try:
        run(args.root, args.file, args.function,
            original=args.original.read_text() if args.original else None, check=args.check)
    except (ValueError, OSError, cfr.LexError) as error:
        print(f'REFUSED: {error}', file=sys.stderr)
        return 1
    return 0


if __name__ == '__main__':
    sys.exit(main())
