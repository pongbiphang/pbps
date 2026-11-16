#!/usr/bin/env python3
"""Tests for the verified family splitter: python3 scripts/change_family_split_test.py."""

import contextlib
import io
import tempfile
import unittest
from pathlib import Path
from unittest import mock

import change_family_split as cfs

MODEL = '''\
pub enum Change {
    Column(ColumnChange),
    Type(TypeChange),
    TableProperty(TablePropertyChange),
    Table(TableChange),
}
pub enum ChangeFamily { Tables, TableProperties, Columns, Types }
pub enum TableChange { First, Last, Add { table: Name }, Drop { table: Name } }
pub enum TablePropertyChange { Set { table: Name } }
pub enum ColumnChange { First, Last, Add { table: Name }, Drop { table: Name } }
pub enum TypeChange { Create }
'''
HEADER = '''\
use pbps_model::*;
mod helpers;
fn untouched() { let s = "Change::Column(fake)"; }
'''


class SplitTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='pbps-split-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.file = 'crates/engine/src/emit.rs'
        self.path = self.root / self.file
        self.write(cfs.cfr.CHANGE_FILE, MODEL)
        self.write('crates/engine/src/emit/helpers.rs', '// Existing helpers.\n')

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def source(self, text, parameters='pg: &Postgres, change: &Change, strategy: Strategy'):
        return (HEADER + f'fn target({parameters}) -> Sql {{\n'
                '    before_match(change);\n    match change {\n' + text + '\n    }\n}\n'
                'fn after() { keep_every_byte(); }\n')

    def split(self, text, parameters='pg: &Postgres, change: &Change, strategy: Strategy'):
        original = self.source(text, parameters)
        self.write(self.file, original)
        report = io.StringIO()
        result = cfs.run(self.root, self.file, 'target', out=report)
        self.original, self.result, self.report = original, result, report.getvalue()
        return result

    def family(self, outer):
        return self.result.module_path(outer).read_text()

    def check(self):
        return cfs.run(self.root, self.file, 'target', original=self.original,
                       check=True, out=io.StringIO())

    def assert_refused_without_writes(self, original, message, name='target'):
        self.write(self.file, original)
        before = {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()}
        with self.assertRaisesRegex(ValueError, message):
            cfs.run(self.root, self.file, name, out=io.StringIO())
        self.assertEqual(before, {p: p.read_bytes() for p in self.root.rglob('*') if p.is_file()})

    def test_family_order_comes_from_change_family_not_outer_enum_or_arm_order(self):
        self.split('''        Change::Column(ColumnChange::First) => make(pg),
        Change::Table(TableChange::First) => make(strategy),
        Change::Type(TypeChange::Create) => make(change),
        Change::TableProperty(TablePropertyChange::Set { table }) => use_table(table),''')
        text = self.path.read_text()
        positions = [text.index(f'Change::{name}({cfs.rust_name(name)})') for name in
                     ['Table', 'TableProperty', 'Column', 'Type']]
        self.assertEqual(positions, sorted(positions))
        self.assertIn('mod table_property;', text)
        self.assertIn('mod r#type;', text)
        self.assertIn('r#type: &TypeChange', self.family('Type'))
        self.assertIn('Change::Type(r#type) => r#type::emit(change, r#type)', text)
        self.check()

    def test_parameters_follow_source_order_and_only_identifier_uses_count(self):
        self.split('''        Change::Table(TableChange::First) => {
            // change strategy unused
            let message = "change strategy unused";
            make(pg, row_defaults, message)
        }
        Change::Column(ColumnChange::First) if allowed(change) => make(strategy),''',
                   'row_defaults: &Defaults, change: &Change, strategy: Strategy, pg: &Postgres, unused: U')
        self.assertEqual([p.name for p in self.result.parameters['Table']],
                         ['row_defaults', 'table', 'pg'])
        self.assertEqual([p.name for p in self.result.parameters['Column']],
                         ['change', 'column', 'strategy'])
        self.assertNotIn('unused: U', self.family('Table'))
        self.assertNotIn('strategy: Strategy', self.family('Table'))
        self.check()

    def test_generic_parameter_types_are_not_split_at_inner_commas(self):
        self.split('        Change::Table(TableChange::First) => make(values),',
                   'values: &BTreeMap<A, Vec<B>>, change: &Change')
        self.assertIn('values: &BTreeMap<A, Vec<B>>', self.family('Table'))
        self.check()

    def test_file_function_parameter_types_and_return_type_are_not_pg_specific(self):
        self.file = 'crates/other-engine/src/preflight.rs'
        self.path = self.root / self.file
        original = self.source('        Change::Table(TableChange::First) => check(dialect),',
                               'dialect: &OtherEngine, change: &Change').replace('-> Sql', '-> Result<()>')
        self.write(self.file, original)
        result = cfs.run(self.root, self.file, 'target', out=io.StringIO())
        text = result.module_path('Table').read_text()
        self.assertIn('dialect: &OtherEngine', text)
        self.assertIn('-> Result<()>', text)
        self.assertNotIn('Postgres', text)
        cfs.run(self.root, self.file, 'target', original=original, check=True, out=io.StringIO())

    def test_whole_family_wildcard_and_binding_are_unwrapped(self):
        self.split('''        Change::Table(inner) => make(inner),
        Change::Column(_) if ready() => make(),''')
        self.assertIn('        inner => make(inner),', self.family('Table'))
        self.assertIn('        _ if ready() => make(),', self.family('Column'))
        self.assertEqual([p.name for p in self.result.parameters['Table']], ['table'])
        self.check()

    def test_same_family_alternatives_keep_guard_and_relative_order(self):
        self.split('''        Change::Table(TableChange::First) if enabled(pg) => first(),
        Change::Column(_) => column(),
        Change::Table(TableChange::Add { table })
        | Change::Table(TableChange::Drop { table }) if accepts(table) => middle(table),
        Change::Table(TableChange::Last) => last(),''')
        text = self.family('Table')
        self.assertLess(text.index('if enabled(pg)'), text.index('if accepts(table)'))
        self.assertLess(text.index('if accepts(table)'), text.index('TableChange::Last'))
        self.assertIn('TableChange::Add { table } | TableChange::Drop { table }', text)
        self.check()

    def test_two_family_projection_keeps_both_orders_guards_and_first_comment(self):
        self.split('''        Change::Column(ColumnChange::First) => column_first(),
        Change::Table(TableChange::First) => table_first(),
        // This reason belongs to the first named family only.
        // Its second line stays attached.
        Change::Table(TableChange::Add { table })
        | Change::Column(ColumnChange::Add { table }) if guarded(change, table) => {
            shared_body(pg, table)
        }
        Change::Table(TableChange::Last) => table_last(),
        Change::Column(ColumnChange::Last) => column_last(),''')
        for outer in ['Table', 'Column']:
            text = self.family(outer)
            self.assertEqual(text.count('guarded(change, table)'), 1)
            self.assertEqual(text.count('shared_body(pg, table)'), 1)
            self.assertLess(text.index(outer + 'Change::First'), text.index('guarded(change, table)'))
            self.assertLess(text.index('guarded(change, table)'), text.index(outer + 'Change::Last'))
        self.assertIn('Its second line stays attached.', self.family('Table'))
        self.assertNotIn('This reason', self.family('Column'))
        self.assertEqual(self.result.shared[0][1], ['Table', 'Column'])
        self.assertIn(f'SPLIT {self.file}:', self.report)
        self.check()

    def test_different_guard_bindings_are_copied_textually_and_reported(self):
        self.split('''        Change::Table(left) | Change::Column(right)
            if allowed(left) => make(left),''')
        self.assertIn('right\n            if allowed(left) => make(left)', self.family('Column'))
        self.assertIn('Column: left is bound only in another family', self.report)
        self.assertIn('Column: possibly unused binding right', self.report)
        self.check()

    def test_three_family_projection_copies_exactly_three_times(self):
        self.split('''        // First family is Column, despite dispatch order.
        Change::Column(_) | Change::Table(_) | Change::Type(_)
            if allowed(change) => make(pg),''')
        self.assertEqual(len(self.result.families), 3)
        for outer in ['Table', 'Column', 'Type']:
            text = self.family(outer)
            self.assertEqual(text.count('if allowed(change)'), 1)
            self.assertEqual(text.count('=> make(pg)'), 1)
            self.assertEqual('First family is Column' in text, outer == 'Column')
        self.check()

    def test_strings_comments_and_nested_matches_are_untouched(self):
        body = ''' {
            // Change::Column(pretend) => fake
            let text = r#"Change::Column(fake) => { match change { } }"#;
            match other {
                Change::Column(_) => use_text(text),
                _ => use_char('}'),
            }
        }'''
        self.split('        Change::Table(TableChange::First) =>' + body)
        self.assertIn('=>' + body, self.family('Table'))
        self.assertEqual(self.result.families, ['Table'])
        self.check()

    def test_line_block_and_trailing_comments_stay_with_their_arm(self):
        self.split('''        /* This block explains the first arm.
           It continues on this line. */
        Change::Table(TableChange::First) => first(), // Trailing first.
        // This explains the second family.
        Change::Column(ColumnChange::First) => second(),''')
        self.assertIn('It continues on this line.', self.family('Table'))
        self.assertIn('// Trailing first.', self.family('Table'))
        self.assertNotIn('Trailing first', self.family('Column'))
        self.assertIn('This explains the second family.', self.family('Column'))
        self.check()

    def test_unresolvable_target_and_nested_only_match_are_refused(self):
        self.assert_refused_without_writes(self.source('        Change::Table(TableChange::First) => make(),'),
                                          'cannot resolve', name='missing')
        original = HEADER + 'fn target(change: &Change) -> Sql { helper(|| { match change { Change::Table(TableChange::First) => make() } }) }'
        self.assert_refused_without_writes(original, 'cannot resolve exactly one top-level match')

    def test_unknown_or_unattributed_patterns_refuse_every_output(self):
        for pattern in ['_', 'Change::Unknown(_)', 'Other::Table(_)', 'Some(Change::Table(_))']:
            with self.subTest(pattern=pattern):
                self.assert_refused_without_writes(self.source(
                    f'        Change::Column(_) => good(),\n        {pattern} => bad(),'),
                    'not a direct Change')

    def test_clashes_with_declared_or_existing_modules_are_refused(self):
        self.assert_refused_without_writes(
            self.source('        Change::Table(TableChange::First) => make(),').replace('mod helpers;', 'mod helpers;\nmod table;'),
            'clashes')
        self.write('crates/engine/src/emit/table.rs', '// Owned elsewhere.\n')
        self.assert_refused_without_writes(self.source('        Change::Table(TableChange::First) => make(),'), 'clashes')

    def test_an_unattached_comment_refuses_instead_of_disappearing(self):
        self.assert_refused_without_writes(self.source('''        // Detached explanation.

        Change::Table(TableChange::First) => make(),'''), 'unattached comments')

    def test_check_detects_wrong_duplication_order_guard_and_comment_ownership(self):
        self.split('''        // First owns this.
        Change::Table(TableChange::First) | Change::Column(ColumnChange::First)
            if allowed(change) => first(pg),
        Change::Table(TableChange::Last) | Change::Column(ColumnChange::Last) => last(),''')
        path = self.result.module_path('Column')
        original = path.read_text()
        mutations = [
            original.replace('first(pg)', 'other(pg)'),
            original.replace('if allowed(change)', 'if disallowed(change)'),
            original.replace('ColumnChange::First', 'ColumnChange::Last'),
            original.replace('        ColumnChange::First', '        // First owns this.\n        ColumnChange::First'),
            original.replace('        ColumnChange::Last => last(),',
                             '        ColumnChange::Last => last(),\n        ColumnChange::Last => last(),'),
            original.replace('change: &Change', 'extra: &Change'),
            original.replace('fn emit(\n', 'fn emit(\n    // Unapproved extra comment.\n'),
            original + '\nfn extra() {}\n',
        ]
        for text in mutations:
            with self.subTest(text=text):
                path.write_text(text)
                with self.assertRaisesRegex(ValueError, 'verification:'):
                    self.check()
        path.write_text(original)
        self.check()

    def test_check_rejects_parent_changes_outside_match_and_mods(self):
        self.split('        Change::Table(TableChange::First) => make(),')
        self.path.write_text(self.path.read_text().replace('keep_every_byte', 'changed'))
        with self.assertRaisesRegex(ValueError, 'parent changed outside'):
            self.check()

    def test_check_accepts_rust_whitespace_but_preserves_literal_and_comment_bytes(self):
        self.split('        Change::Table(TableChange::First) => make("unchanged text"),')
        path = self.result.module_path('Table')
        path.write_text(path.read_text().replace('        TableChange::First => make', '    TableChange::First  =>  make'))
        self.check()
        path.write_text(path.read_text().replace('unchanged text', 'unchanged  text'))
        with self.assertRaisesRegex(ValueError, 'arm content'):
            self.check()

    def test_check_accepts_formatter_removing_a_signature_trailing_comma(self):
        self.split('        Change::Table(TableChange::First) => make(),')
        path = self.result.module_path('Table')
        path.write_text(path.read_text().replace('fn emit(\n    table: &TableChange,\n)',
                                                'fn emit(table: &TableChange)'))
        self.check()

    def test_check_accepts_formatter_eliding_the_braces_of_a_one_expression_arm(self):
        self.split('        Change::Table(TableChange::First) => {\n            make()\n        }')
        path = self.result.module_path('Table')
        moved = path.read_text()
        path.write_text(moved.replace('TableChange::First => {\n            make()\n        }', 'TableChange::First => make(),'))
        self.check()

    def test_check_still_refuses_a_changed_block_or_an_unbraced_statement_block(self):
        self.split('        Change::Table(TableChange::First) => {\n            let x = make();\n            x\n        }\n'
                   '        Change::Column(ColumnChange::First) => {\n            // Why.\n            make()\n        }')
        table, column = self.result.module_path('Table'), self.result.module_path('Column')
        for path, old, new in [
            (table, '{\n            let x = make();\n            x\n        }', 'make(),'),
            (column, '{\n            // Why.\n            make()\n        }', 'make(),'),
            (table, 'x\n        }', 'other\n        }'),
        ]:
            with self.subTest(new=new, old=old):
                before = path.read_text()
                path.write_text(before.replace(old, new))
                self.assertNotEqual(before, path.read_text())
                with self.assertRaisesRegex(ValueError, 'arm content'):
                    self.check()
                path.write_text(before)
        self.check()

    def test_a_parent_without_out_of_line_modules_takes_them_after_its_last_use(self):
        header = HEADER.replace('mod helpers;\n', '')
        self.assertNotEqual(header, HEADER)
        original = (header + 'fn target(change: &Change) -> Sql {\n    match change {\n'
                    '        Change::Table(TableChange::First) => make(),\n        Change::Column(ColumnChange::First) => make(),\n'
                    '    }\n}\n')
        self.write(self.file, original)
        self.original = original
        cfs.run(self.root, self.file, 'target', out=io.StringIO())
        parent = self.path.read_text()
        last_use = parent.index('use pbps_model::*;\n')
        declared = parent.index('mod column;\nmod table;\n')
        self.assertGreater(declared, last_use)
        self.assertLess(declared, parent.index('fn target'))
        self.check()

    def test_a_family_answered_by_one_whole_family_arm_stays_in_the_dispatch(self):
        self.split('        // Not on this engine.\n        Change::Table(_) => refuse(),\n'
                   '        Change::Column(_) => make(),\n        Change::Column(ColumnChange::First) => never(),')
        self.assertFalse(self.result.module_path('Table').exists())
        self.assertTrue(self.result.module_path('Column').exists())
        parent = self.path.read_text()
        self.assertIn('        // Not on this engine.\n        Change::Table(_) => refuse(),\n', parent)
        self.assertNotIn('mod table;', parent)
        self.check()
        self.path.write_text(parent.replace('refuse()', 'other()'))
        with self.assertRaisesRegex(ValueError, 'incorrect dispatch for Table'):
            self.check()

    def test_an_inline_family_may_share_its_name_with_a_parameter(self):
        # No binding is made for an inline family, so nothing can clash.
        self.split('        Change::Table(_) => refuse(table),\n'
                   '        Change::Column(ColumnChange::First) => make(),',
                   parameters='change: &Change, table: &str')
        self.assertFalse(self.result.module_path('Table').exists())
        self.check()

    def test_a_moved_family_still_refuses_a_parameter_of_its_name(self):
        self.assert_refused_without_writes(
            self.source('        Change::Table(TableChange::First) => make(),',
                        parameters='change: &Change, table: &str'),
            'clashes with a source parameter')

    def test_a_guarded_or_shared_whole_family_arm_still_moves(self):
        self.split('        Change::Table(_) if ready(change) => make(),\n        Change::Table(_) => wait(),')
        self.assertTrue(self.result.module_path('Table').exists())
        self.check()

    def test_generics_go_to_a_family_whose_parameters_name_them(self):
        original = (HEADER + "fn target<'v, 'p: 'v>(change: &Change, names: &impl Names<'v, 'p>) -> Sql {\n"
                    '    match change {\n'
                    '        Change::Table(TableChange::First) => first(names),\n'
                    '        Change::Column(ColumnChange::First) => other(),\n    }\n}\n')
        self.write(self.file, original)
        self.original = original
        self.result = cfs.run(self.root, self.file, 'target', out=io.StringIO())
        self.assertIn("fn emit<'v, 'p: 'v>(\n", self.family('Table'))
        self.assertIn('fn emit(\n', self.family('Column'))
        self.check()

    def test_a_generic_named_only_in_an_arm_body_is_still_declared(self):
        original = (HEADER + "fn target<'v>(change: &Change, names: &Names<'v>) -> Sql {\n"
                    '    match change {\n'
                    "        Change::Table(TableChange::First) => { let x: &'v str = pick(); make(x) }\n"
                    '        Change::Column(ColumnChange::First) => other(names),\n    }\n}\n')
        self.write(self.file, original)
        self.original = original
        self.result = cfs.run(self.root, self.file, 'target', out=io.StringIO())
        self.assertIn("fn emit<'v>(\n", self.family('Table'))
        self.check()

    def test_a_type_parameter_no_family_parameter_names_is_refused(self):
        self.assert_refused_without_writes(
            HEADER + 'fn target<T: Probe>(change: &Change, probe: T) -> Sql {\n    match change {\n'
            '        Change::Table(TableChange::First) => T::default().run(),\n'
            '        Change::Column(ColumnChange::First) => probe.run(),\n    }\n}\n',
            'could not be inferred')

    def test_a_match_that_is_not_the_tail_is_refused(self):
        # A moved `return` or `?` would leave the family function and run
        # what follows the match: a different program.
        self.assert_refused_without_writes(
            HEADER + 'fn target(change: &Change) -> Sql {\n    let x = match change {\n'
            '        Change::Table(TableChange::First) => make()?,\n    };\n    x\n}\n',
            'tail expression')
        self.assert_refused_without_writes(
            HEADER + 'fn target(change: &Change) -> Sql {\n    match change {\n'
            '        Change::Table(TableChange::First) => make()?,\n    }\n    after()\n}\n',
            'tail expression')

    def test_the_parent_file_names_the_family_function(self):
        self.file = 'crates/engine/src/preflight.rs'
        self.path = self.root / self.file
        self.write('crates/engine/src/preflight/helpers.rs', '// Existing helpers.\n')
        self.split('        Change::Table(TableChange::First) => make(),')
        table = self.family('Table')
        self.assertIn('pub(super) fn preflight(', table)
        self.assertIn('//! Probes the table family of changes before apply', table)
        self.assertIn('table::preflight(table)', self.path.read_text())
        self.check()

    def test_a_parent_without_wording_is_refused(self):
        self.file = 'crates/engine/src/rows.rs'
        self.path = self.root / self.file
        self.assert_refused_without_writes(self.source('        Change::Table(TableChange::First) => make(),'),
                                           'no family-module wording')

    def test_a_comment_between_alternatives_moves_with_the_next_one(self):
        self.split('''        // Everything else is free.
        Change::Table(TableChange::First)
        // Columns: nothing to count.
        | Change::Column(ColumnChange::First)
        | Change::Table(TableChange::Last)
        // The last column too.
        | Change::Column(ColumnChange::Last) => free(),''')
        table, column = self.family('Table'), self.family('Column')
        self.assertIn('Everything else is free.', table)
        self.assertNotIn('Columns: nothing', table)
        self.assertNotIn('Everything else', column)
        # The first column alternative's comment opens the arm; the later one
        # stays in the pattern, before its alternative.
        self.assertLess(column.index('Columns: nothing'), column.index('ColumnChange::First'))
        self.assertLess(column.index('ColumnChange::First'), column.index('The last column too.'))
        self.assertLess(column.index('The last column too.'), column.index('ColumnChange::Last'))
        self.check()

    def test_check_accepts_rustfmt_trailing_commas_and_nested_arm_braces(self):
        self.split('        Change::Table(TableChange::First) => { let x = S { a, b }; match x { A => { ok() } B => no(), } }')
        path = self.result.module_path('Table')
        moved = path.read_text()
        for old, new in [('S { a, b }', 'S { a, b, }'),
                         ('A => { ok() } B => no(), }', 'A => ok(), B => no() }')]:
            with self.subTest(new=new):
                self.assertIn(old, moved)
                path.write_text(moved.replace(old, new))
                self.check()
        for old, new in [('S { a, b }', 'S { a; b }'),
                         ('A => { ok() }', 'A => { ok(); }'),
                         ('A => { ok() }', 'A => { /* why */ ok() }')]:
            with self.subTest(new=new):
                path.write_text(moved.replace(old, new))
                with self.assertRaisesRegex(ValueError, 'arm content'):
                    self.check()
        path.write_text(moved)

    def test_a_one_element_tuple_keeps_its_comma(self):
        fp = cfs.fingerprint
        self.assertNotEqual(fp('(a,)'), fp('(a)'))
        self.assertEqual(fp('f(a, b,)'), fp('f(a, b)'))
        self.assertEqual(fp('g((a,), b,)'), fp('g((a,), b)'))

    def test_a_parent_with_neither_modules_nor_uses_is_refused(self):
        self.assert_refused_without_writes(
            'fn target(change: &Change) -> Sql {\n    match change {\n        Change::Table(TableChange::First) => make(),\n    }\n}\n',
            'no module declaration or top-level use')

    def test_failed_post_write_verification_restores_parent_and_removes_outputs(self):
        original = self.source('        Change::Table(TableChange::First) => make(),')
        self.write(self.file, original)
        original_verify = cfs.Split.verify
        calls = []

        def fail_second(split, *args, **kwargs):
            calls.append(True)
            if len(calls) == 2:
                raise ValueError('injected verification failure')
            return original_verify(split, *args, **kwargs)

        with mock.patch.object(cfs.Split, 'verify', fail_second):
            with self.assertRaisesRegex(ValueError, 'injected'):
                cfs.run(self.root, self.file, 'target', out=io.StringIO())
        self.assertEqual(self.path.read_text(), original)
        self.assertFalse((self.path.with_suffix('') / 'table.rs').exists())
        self.assertTrue((self.path.with_suffix('') / 'helpers.rs').exists())

    def test_cli_check_requires_original_and_returns_nonzero_on_unresolved_target(self):
        self.write(self.file, self.source('        Change::Table(TableChange::First) => make(),'))
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit) as error:
                cfs.main([str(self.root), '--file', self.file, '--function', 'target', '--check'])
            self.assertEqual(error.exception.code, 2)
            self.assertEqual(cfs.main([str(self.root), '--file', self.file, '--function', 'missing']), 1)


if __name__ == '__main__':
    unittest.main()
