#!/usr/bin/env python3
"""Tests for the all-family collapse: python3 scripts/change_family_collapse_test.py."""

import io
import tempfile
import unittest
from pathlib import Path

import change_family_collapse as cfc

MODEL = '''\
pub enum Change {
    Extension(ExtensionChange),
    Role(RoleChange),
    Type(TypeChange),
    Search(SearchChange),
}
pub enum ExtensionChange { Create { uid: Uid }, Drop { uid: Uid }, Rebuild(Box<Thing>), Marker }
pub enum RoleChange { Create { uid: Uid } }
pub enum TypeChange { Create { uid: Uid } }
pub enum SearchChange { Create { uid: Uid } }

/// Why this macro existed.
#[macro_export]
macro_rules! change_family {
    (extensions) => { $crate::Change::Extension($crate::ExtensionChange::Marker) };
}

/// Why catalog patterns existed.
#[macro_export]
macro_rules! catalog_change {
    () => { $crate::change_family!(extensions) };
}

impl Change {
    pub fn family(&self) -> Family {
        match self {
            crate::change_family!(extensions) => Family::Extensions,
            crate::change_family!(roles) => Family::Roles,
            crate::change_family!(types) => Family::Types,
            crate::change_family!(search) => Family::Search,
        }
    }
    pub fn catalog_object(&self) -> Option<Object> {
        match self { crate::catalog_change!() => Some(Object) }
    }
}
'''
MEMBERS = ['Change::Extension(ExtensionChange::Create { .. })',
           'Change::Extension(ExtensionChange::Drop { .. })',
           'Change::Extension(ExtensionChange::Rebuild(..))',
           'Change::Extension(ExtensionChange::Marker)']
HEADER = 'use pbps_model::{Change, ExtensionChange};\n'


class CollapseTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix='pbps-collapse-')
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.write(cfc.cfr.CHANGE_FILE, MODEL)
        self.write('crates/pbps-model/src/lib.rs', 'pub mod change;\npub use change::*;\n')
        self.file = 'crates/consumer/src/lib.rs'

    def write(self, name, src):
        p = self.root / name
        p.parent.mkdir(parents=True, exist_ok=True)
        p.write_text(src)
        return p

    def run_tool(self, src, header=HEADER, write=True):
        path = self.write(self.file, header + src)
        out = io.StringIO()
        project = cfc.run(self.root, write=write, out=out)
        return path.read_text(), project, out.getvalue()

    def pattern(self, members=MEMBERS, suffix=''):
        return 'fn f(c: Change) { match c { ' + ' | '.join(members) + suffix + ' => (), _ => () } }\n'

    def test_macros_use_visible_change_spelling_and_remove_definitions(self):
        for macro in ['change_family', 'pbps_model::change_family']:
            with self.subTest(macro=macro):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                text, project, _ = self.run_tool('fn f(c: C) { match c { ' + macro + '!(extensions) => (), _ => () } }',
                                                header='use pbps_model::Change as C;\n')
                self.assertFalse(project.reports)
                self.assertIn('C::Extension(_)', text)
                model = (self.root / cfc.cfr.CHANGE_FILE).read_text()
                self.assertNotIn('macro_rules!', model)
                self.assertIn('Change::Extension(_) => Family::Extensions', model)
                self.assertIn('Change::Role(_) | Change::Type(_) | Change::Search(_) | Change::Extension(_)', model)
                self.assertNotIn('Why this macro', model)

    def test_qualified_macro_without_change_import_preserves_prefix(self):
        text, project, _ = self.run_tool('fn f(c: C) { matches!(c, pbps_model::change_family!(extensions)) }', header='')
        self.assertFalse(project.reports)
        self.assertIn('pbps_model::Change::Extension(_)', text)

    def test_crate_and_dollar_crate_macros(self):
        self.file = 'crates/pbps-model/src/user.rs'
        text, project, _ = self.run_tool('macro_rules! m { () => { $crate::change_family!(extensions) }; }\n'
                                        'fn f(c: C) { matches!(c, crate::change_family!(extensions)) }', header='')
        self.assertFalse(project.reports)
        self.assertIn('$crate::Change::Extension(_)', text)
        self.assertIn('crate::Change::Extension(_)', text)

    def test_catalog_members_and_whole_catalog(self):
        for family in cfc.CATALOG:
            self.write(cfc.cfr.CHANGE_FILE, MODEL)
            text, project, _ = self.run_tool('fn f(c: Change) { matches!(c, catalog_change!(' + family + ')) }')
            self.assertFalse(project.reports)
            self.assertIn('Change::' + cfc.FAMILIES[family] + '(_)', text)
        self.write(cfc.cfr.CHANGE_FILE, MODEL)
        text, _, _ = self.run_tool('fn f(c: Change) { matches!(c, catalog_change!()) }')
        self.assertIn('Change::Role(_) | Change::Type(_) | Change::Search(_) | Change::Extension(_)', text)

    def test_complete_set_collapses_at_first_member_and_prunes_import(self):
        members = [MEMBERS[0], 'Change::Role(_)', *MEMBERS[1:]]
        text, project, _ = self.run_tool(self.pattern(members))
        self.assertFalse(project.reports)
        self.assertIn('Change::Extension(_) | Change::Role(_)', text)
        self.assertNotIn('ExtensionChange', text)
        self.assertEqual(project.counts['c_complete_family'], 1)

    def test_matches_pattern_collapses_but_partial_pattern_stays(self):
        src = 'fn f(c: Change) { matches!(c, ' + ' | '.join(MEMBERS) + ') }'
        text, _, _ = self.run_tool(src)
        self.assertIn('matches!(c, Change::Extension(_))', text)
        partial = 'fn g(c: Change) { matches!(c, ' + ' | '.join(MEMBERS[:2]) + ') }'
        text, _, _ = self.run_tool(partial)
        self.assertEqual(text, HEADER + partial)

    def test_identical_wrappers_and_nested_or_patterns(self):
        for wrapper in ['Some({})', '&{}', '({}, _)', 'Some((_, {}))']:
            with self.subTest(wrapper=wrapper):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                text, _, _ = self.run_tool(self.pattern([wrapper.format(m) for m in MEMBERS]))
                self.assertIn(wrapper.format('Change::Extension(_)'), text)
                self.assertNotIn('ExtensionChange::', text)
        text, _, _ = self.run_tool(self.pattern(['Some(' + ' | '.join(MEMBERS) + ')']))
        self.assertIn('Some(Change::Extension(_))', text)

    def test_multiline_trailing_comma_and_alias_paths(self):
        members = [m.replace('Change::Extension', 'C::Extension') for m in MEMBERS]
        members[0] = members[0].replace(' })', ' },\n)')
        text, project, _ = self.run_tool(self.pattern(members), header='use pbps_model::{Change as C, ExtensionChange};\n')
        self.assertFalse(project.reports)
        self.assertIn('C::Extension(_)', text)

    def test_outer_and_inner_paths_resolve_independently(self):
        for members in [[m.replace('ExtensionChange::', 'pbps_model::ExtensionChange::') for m in MEMBERS],
                        [m.replace('Change::Extension', 'pbps_model::Change::Extension') for m in MEMBERS]]:
            with self.subTest(members=members):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                text, project, _ = self.run_tool(self.pattern(members))
                self.assertFalse(project.reports)
                self.assertNotIn('ExtensionChange::', text)

    def test_tuple_wildcards_do_not_bind_payloads(self):
        text, _, _ = self.run_tool(self.pattern([m.replace('Rebuild(..)', 'Rebuild(_)') for m in MEMBERS]))
        self.assertIn('Change::Extension(_)', text)

    def test_a_fixed_family_member_in_another_tuple_slot_is_preserved(self):
        members = ['(' + m + ', Change::Role(RoleChange::Create { .. }))' for m in MEMBERS]
        text, project, _ = self.run_tool(self.pattern(members), header=HEADER + 'use pbps_model::RoleChange;\n')
        self.assertFalse(project.reports)
        self.assertIn('(Change::Extension(_), Change::Role(RoleChange::Create { .. }))', text)
        self.assertNotIn('ExtensionChange::', text)

    def test_import_only_files_do_not_refer_to_removed_macros(self):
        text, project, _ = self.run_tool('use pbps_model::catalog_change;\n', header='')
        self.assertFalse(project.reports)
        self.assertEqual(text, '')

    def test_adjacent_unused_grouped_imports_are_removed_together(self):
        text, project, _ = self.run_tool(self.pattern(), header='use pbps_model::{Change, ExtensionChange, catalog_change};\n')
        self.assertFalse(project.reports)
        self.assertNotIn('ExtensionChange', text)
        self.assertNotIn('catalog_change', text)
        self.assertIn('use pbps_model::{Change};', text)

    def test_child_import_does_not_keep_unused_parent_import(self):
        src = self.pattern() + '''
mod tests {
    use pbps_model::ExtensionChange;
    fn member() -> ExtensionChange { ExtensionChange::Marker }
}
'''
        text, project, _ = self.run_tool(src)
        self.assertFalse(project.reports)
        self.assertNotIn('use pbps_model::{Change, ExtensionChange}', text)
        self.assertIn('    use pbps_model::ExtensionChange;', text)
        self.assertEqual(text.count('use pbps_model::ExtensionChange;'), 1)

    def test_qualified_reference_does_not_use_a_bare_import(self):
        src = self.pattern() + '\nfn member() -> pbps_model::ExtensionChange { pbps_model::ExtensionChange::Marker }'
        text, project, _ = self.run_tool(src)
        self.assertFalse(project.reports)
        self.assertIn('use pbps_model::{Change};', text)
        self.assertIn('pbps_model::ExtensionChange::Marker', text)

    def test_parent_import_used_through_super_is_kept(self):
        for child_import in ['use super::*;', 'use super::ExtensionChange;']:
            with self.subTest(child_import=child_import):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                src = self.pattern() + '\nmod tests { ' + child_import + ' fn member() -> ExtensionChange { ExtensionChange::Marker } }'
                text, project, _ = self.run_tool(src)
                self.assertFalse(project.reports)
                self.assertIn(HEADER, text)
        text, project, _ = self.run_tool(self.pattern() + '\nmod tests { fn member() -> super::ExtensionChange { super::ExtensionChange::Marker } }')
        self.assertFalse(project.reports)
        self.assertIn(HEADER, text)

    def test_unused_alias_qualified_leaf_and_module_path_are_pruned(self):
        for header in ['use pbps_model::{Change, ExtensionChange as EC};\n',
                       'use pbps_model::{Change, change::ExtensionChange as EC};\n',
                       'use pbps_model::{Change, change::{ExtensionChange}};\n',
                       'use pbps_model::{Change, change::{ExtensionChange,}};\n',
                       'use pbps_model::Change;\nuse pbps_model::change as changes;\n']:
            with self.subTest(header=header):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                text, project, _ = self.run_tool('fn f(c: Change) { matches!(c, Change::Extension(_)) }', header=header)
                self.assertFalse(project.reports)
                self.assertNotIn('ExtensionChange', text)
                self.assertNotIn('as changes', text)

    def test_used_alias_module_path_and_public_reexports_are_kept(self):
        src = 'fn member() -> EC { EC::Marker }'
        text, project, _ = self.run_tool(src, header='use pbps_model::ExtensionChange as EC;\n')
        self.assertFalse(project.reports)
        self.assertIn('ExtensionChange as EC', text)
        text, project, _ = self.run_tool('fn member() -> changes::ExtensionChange { changes::ExtensionChange::Marker }',
                                        header='use pbps_model::change as changes;\n')
        self.assertFalse(project.reports)
        self.assertIn('use pbps_model::change as changes;', text)
        text, project, _ = self.run_tool('', header='pub use pbps_model::ExtensionChange;\n')
        self.assertFalse(project.reports)
        self.assertIn('pub use pbps_model::ExtensionChange;', text)

    def test_unused_family_variant_glob_is_pruned_but_used_one_is_kept(self):
        header = 'use pbps_model::ExtensionChange::*;\n'
        text, project, _ = self.run_tool('', header=header)
        self.assertFalse(project.reports)
        self.assertEqual(text, '')
        text, project, _ = self.run_tool('fn member() -> C { Marker }', header=header)
        self.assertFalse(project.reports)
        self.assertIn(header, text)

    def test_enum_docs_name_the_family_and_its_holder_without_macro_prose(self):
        original = MODEL.replace('pub enum ExtensionChange',
                                 '/// The kinds of change of the `extensions` family (DEC-2790.1), the enum\n'
                                 '/// `change_family!(extensions)` names. [`Change::Extension`] holds one. It is written\n'
                                 '/// flat, tagged by `op`, whatever holds it (`change/wire.rs`).\n'
                                 'pub enum ExtensionChange')
        self.write(cfc.cfr.CHANGE_FILE, original)
        self.run_tool(self.pattern())
        text = (self.root / cfc.cfr.CHANGE_FILE).read_text()
        self.assertIn('family (DEC-2790.1).\n/// [`Change::Extension`] holds one.', text)
        self.assertNotIn('the enum\n///', text)

    def test_catalog_comment_lists_all_four_families(self):
        text, project, _ = self.run_tool('// Roles, types and extensions (`catalog_change!`).\n', header='')
        self.assertFalse(project.reports)
        self.assertIn('Roles, types, search objects and extensions (catalog family patterns).', text)

    def test_incomplete_binding_guard_at_and_different_wrappers_are_retained(self):
        cases = [self.pattern(MEMBERS[:-1]),
                 self.pattern([MEMBERS[0].replace('..', 'uid, ..'), *MEMBERS[1:]]),
                 self.pattern(suffix=' if enabled()'),
                 self.pattern(['x @ ' + m for m in MEMBERS]),
                 self.pattern(['Some(' + MEMBERS[0] + ')', *MEMBERS[1:]]),
                 self.pattern(['(' + m + ', bound)' for m in MEMBERS]),
                 self.pattern([MEMBERS[0], MEMBERS[1].replace('Change::Extension', 'pbps_model::Change::Extension'), *MEMBERS[2:]])]
        for src in cases:
            with self.subTest(src=src):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                text, _, _ = self.run_tool(src)
                self.assertEqual(text, HEADER + src)

    def test_comment_between_alternatives_is_preserved(self):
        src = self.pattern().replace(' | ', ' | /* why this member */ ', 1)
        text, _, _ = self.run_tool(src)
        self.assertEqual(text, HEADER + src)

    def test_leading_enumeration_reason_preserves_members_but_rewrites_macros(self):
        for comment in ['// Listed rather than a wildcard, so new kinds are decided here.',
                        '/* Listed rather than a wildcard, so new kinds are decided here. */',
                        '// Listed rather than a\n// wildcard, so new kinds are decided here.']:
            with self.subTest(comment=comment):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                src = ('fn f(c: Change) { match c {\n' + comment + '\n' +
                       ' | '.join(MEMBERS) + ' | pbps_model::change_family!(roles) => (),\n_ => ()\n} }')
                text, project, out = self.run_tool(src)
                self.assertFalse(project.reports)
                self.assertEqual(project.counts['c_complete_family'], 0)
                for member in MEMBERS:
                    self.assertIn(member, text)
                self.assertIn(' | Change::Role(_) => ()', text)
                self.assertNotIn('change_family!', text)
                self.assertEqual(len(project.skipped_arms), 1)
                self.assertIn('SKIP rule c: leading comment', out)

    def test_leading_enumeration_reason_also_preserves_nested_or_patterns(self):
        src = ('fn f(c: C) { match c {\n// Listed rather than a wildcard.\nSome(' +
               ' | '.join(MEMBERS) + ') => (),\n_ => ()\n} }')
        text, project, _ = self.run_tool(src)
        self.assertFalse(project.reports)
        self.assertEqual(text, HEADER + src)
        self.assertEqual(len(project.skipped_arms), 1)

    def test_inline_leading_comment_is_attached_to_the_first_arm(self):
        for comment in ['/* Listed rather than a wildcard. */', '// Listed rather than a wildcard.\n']:
            with self.subTest(comment=comment):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                src = 'fn f(c: Change) { match c { ' + comment + ' | '.join(MEMBERS) + ' => (), _ => () } }'
                text, project, _ = self.run_tool(src)
                self.assertEqual(text, HEADER + src)
                self.assertEqual(len(project.skipped_arms), 1)

    def test_unrelated_or_neighboring_comments_do_not_preserve_member_listing(self):
        for prefix in ['// This arm treats the family alike.\n',
                       'Change::Role(_) => (), // Listed rather than a wildcard.\n']:
            with self.subTest(prefix=prefix):
                self.write(cfc.cfr.CHANGE_FILE, MODEL)
                src = 'fn f(c: Change) { match c {\n' + prefix + ' | '.join(MEMBERS) + ' => (),\n_ => ()\n} }'
                text, project, _ = self.run_tool(src)
                self.assertFalse(project.reports)
                self.assertEqual(project.counts['c_complete_family'], 1)
                self.assertIn('Change::Extension(_) => ()', text)
                self.assertFalse(project.skipped_arms)
        text, project, _ = self.run_tool('// Exhaustive rather than a wildcard.\n' + self.pattern())
        self.assertFalse(project.reports)
        self.assertIn('Change::Extension(_)', text)
        self.assertFalse(project.skipped_arms)

    def test_skip_report_includes_every_matching_arm(self):
        pattern = ' | '.join(MEMBERS)
        src = ('fn f(c: Change) { match c {\n// Listed rather than a wildcard.\n' + pattern +
               ' => (),\n// Again, rather than a wildcard.\n' + pattern + ' => (),\n_ => ()\n} }')
        text, project, out = self.run_tool(src)
        self.assertEqual(text, HEADER + src)
        self.assertEqual(len(project.skipped_arms), 2)
        self.assertEqual(out.count('SKIP rule c: leading comment'), 2)

    def test_strings_are_not_macro_sites(self):
        src = 'const S: &str = "change_family!(extensions) catalog_change!()";\n'
        text, project, _ = self.run_tool(src, header='')
        self.assertFalse(project.reports)
        self.assertEqual(text, src)

    def test_classifier_source_pin_is_migrated_with_the_macros(self):
        self.file = 'crates/pbps-model/tests/catalog_object.rs'
        text, project, _ = self.run_tool('''\
fn every_variant_is_listed_in_exactly_one_family() {
    let families = body_of("macro_rules! change_family", "\\n}\\n");
}
''')
        self.assertFalse(project.reports)
        self.assertIn('for family in &change.variants', text)
        self.assertNotIn('macro_rules! change_family', text)

    def test_adjacent_block_arms_and_function_call_bodies(self):
        src = 'fn f(c: Change) { match c { Change::Role(_) => {} ' + ' | '.join(MEMBERS) + ' => f(), _ => {} } }'
        text, project, _ = self.run_tool(src)
        self.assertFalse(project.reports)
        self.assertIn('Change::Extension(_) => f()', text)

    def test_complete_sets_do_not_cross_match_arms_or_matches_invocations(self):
        src = 'fn f(c: Change) { match c { ' + ' | '.join(MEMBERS[:2]) + ' => (), ' + ' | '.join(MEMBERS[2:]) + ' => (), _ => () } }'
        text, _, _ = self.run_tool(src)
        self.assertEqual(text, HEADER + src)

    def test_two_families_in_one_pattern_collapse_independently(self):
        members = [MEMBERS[0], 'Change::Role(RoleChange::Create { .. })', *MEMBERS[1:]]
        text, project, _ = self.run_tool(self.pattern(members), header=HEADER + 'use pbps_model::RoleChange;\n')
        self.assertFalse(project.reports)
        self.assertIn('Change::Extension(_) | Change::Role(_)', text)
        self.assertEqual(project.counts['c_complete_family'], 2)

    def test_comments_in_untouched_files_and_unknown_macro_arguments(self):
        src = 'fn f(c: Change) { matches!(c, change_family!(missing)) }'
        text, project, _ = self.run_tool(src)
        self.assertTrue(project.reports)
        self.assertEqual(text, HEADER + src)

    def test_unknown_macro_expansion_scope_is_reported(self):
        text, project, _ = self.run_tool('custom!(change_family!(extensions));')
        self.assertTrue(project.reports)
        self.assertIn('change_family!(extensions)', text)

    def test_unattributed_sites_abort_every_file_and_report_each(self):
        path = self.write(self.file, 'fn f(c: C) { matches!(c, change_family!(extensions)) }')
        other = self.write('crates/other/src/lib.rs', 'fn f(c: C) { matches!(c, catalog_change!()) }')
        before = {p: p.read_bytes() for p in self.root.rglob('*.rs')}
        project = cfc.run(self.root, out=io.StringIO())
        self.assertEqual(len(project.reports), 2)
        for p, content in before.items():
            self.assertEqual(p.read_bytes(), content)
        self.assertTrue(path.exists() and other.exists())

    def test_unattributed_complete_pattern_is_reported(self):
        text, project, _ = self.run_tool(self.pattern(), header='')
        self.assertEqual(len(project.reports), 4)
        self.assertNotIn('Change::Extension(_)', text)

    def test_unrelated_change_enum_is_not_collapsed(self):
        src = 'enum Change {}\n' + self.pattern()
        text, project, _ = self.run_tool(src, header='use pbps_model::ExtensionChange;\n')
        self.assertFalse(project.reports)
        self.assertEqual(text, 'use pbps_model::ExtensionChange;\n' + src)

    def test_second_run_and_check_mode_write_nothing(self):
        src = self.pattern()
        text, project, _ = self.run_tool(src, write=False)
        self.assertEqual(text, HEADER + src)
        self.assertTrue(project.changed)
        cfc.run(self.root, out=io.StringIO())
        second = cfc.run(self.root, out=io.StringIO())
        self.assertFalse(second.changed)
        self.assertFalse(second.counts)

    def test_non_crates_files_are_untouched(self):
        path = self.write('other.rs', self.pattern())
        self.run_tool(self.pattern())
        self.assertEqual(path.read_text(), self.pattern())


if __name__ == '__main__':
    unittest.main()
