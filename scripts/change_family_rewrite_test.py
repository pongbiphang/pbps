#!/usr/bin/env python3
"""Tests for scripts/change_family_rewrite.py. Run: python3 scripts/change_family_rewrite_test.py"""

import io
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import change_family_rewrite as cfr  # noqa: E402

CHANGE_RS = """\
pub enum Change {
    CreateTable { uid: Uid },
    CreateExtension { uid: Uid },
    DropExtension { uid: Uid },
    RebuildThing(Box<Thing>),
    Marker,
}

#[macro_export]
macro_rules! change_family {
    (tables) => {
        $crate::Change::CreateTable { .. }
    };
    (extensions) => {
        $crate::Change::CreateExtension { .. }
            | $crate::Change::DropExtension { .. }
            | $crate::Change::RebuildThing(..)
            | $crate::Change::Marker
    };
}
"""


class Tree(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pbps-family-rewrite-")
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.write("crates/pbps-model/src/change.rs", CHANGE_RS)
        self.write("crates/pbps-model/src/lib.rs", "pub mod change;\nmod user;\n")

    def write(self, name, text):
        path = self.root / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        return path

    def read(self, name):
        return (self.root / name).read_text()

    def run_tool(self, write=True):
        out = io.StringIO()
        project = cfr.run(self.root, "extensions", "Extension", "ExtensionChange", write, out=out)
        self.last_project = project
        return project, out.getvalue()

    def rewrite(self, body, name="crates/pbps-cli/src/a.rs", header="use pbps_model::Change;\n"):
        self.write(name, header + body)
        project, out = self.run_tool()
        return self.read(name), project, out


class Reading(Tree):
    def test_kinds_and_shapes_come_from_change_rs(self):
        kinds, shapes = cfr.read_family(self.root, "extensions", "Extension", "ExtensionChange")
        self.assertEqual(kinds, ["CreateExtension", "DropExtension", "RebuildThing", "Marker"])
        self.assertEqual(shapes["CreateExtension"], "struct")
        self.assertEqual(shapes["RebuildThing"], "tuple")
        self.assertEqual(shapes["Marker"], "unit")

    def test_an_unknown_family_is_refused(self):
        with self.assertRaises(SystemExit):
            cfr.read_family(self.root, "nothing", "N", "NChange")

    def test_a_family_already_moved_reads_the_same_kinds(self):
        moved = CHANGE_RS.replace(
            "$crate::Change::CreateExtension { .. }\n            | $crate::Change::DropExtension { .. }",
            "$crate::Change::Extension($crate::ExtensionChange::CreateExtension { .. })\n"
            "            | $crate::Change::Extension($crate::ExtensionChange::DropExtension { .. })",
        )
        self.write("crates/pbps-model/src/change.rs", moved)
        kinds, _ = cfr.read_family(self.root, "extensions", "Extension", "ExtensionChange")
        self.assertIn("CreateExtension", kinds)
        self.assertIn("DropExtension", kinds)
        self.assertNotIn("Extension", kinds)


class Positive(Tree):
    def test_struct_pattern_with_payload(self):
        text, _, _ = self.rewrite("fn f(c: &Change) { if let Change::CreateExtension { uid, .. } = c { g(uid) } }\n")
        self.assertIn(
            "Change::Extension(ExtensionChange::CreateExtension { uid, .. })",
            text,
        )
        self.assertIn("use pbps_model::ExtensionChange;", text)

    def test_tuple_variant_and_expression(self):
        text, _, _ = self.rewrite("fn f(x: Box<Thing>) -> Change { Change::RebuildThing(x) }\n")
        self.assertIn("Change::Extension(ExtensionChange::RebuildThing(x))", text)

    def test_unit_variant(self):
        text, _, _ = self.rewrite("fn f(c: Change) -> bool { c == Change::Marker }\n")
        self.assertIn("c == Change::Extension(ExtensionChange::Marker) }", text)

    def test_nested_payload_is_balanced(self):
        text, _, _ = self.rewrite("fn f() -> Change { Change::CreateExtension { uid: mk(a, { b }), } }\n")
        self.assertIn(
            "Change::Extension(ExtensionChange::CreateExtension { uid: mk(a, { b }), })",
            text,
        )

    def test_or_pattern_alternatives_each_rewritten(self):
        text, _, _ = self.rewrite(
            "fn f(c: &Change) -> u8 {\n    match c {\n        Change::CreateExtension { .. }\n"
            "        | Change::DropExtension { .. } => 1,\n        _ => 0,\n    }\n}\n"
        )
        self.assertIn("Change::Extension(ExtensionChange::CreateExtension { .. })\n        | Change::Extension(ExtensionChange::DropExtension { .. }) => 1", text)

    def test_two_sites_in_one_expression(self):
        text, project, _ = self.rewrite(
            "fn f() -> (Change, Change) { (Change::DropExtension { uid: u() }, Change::DropExtension { uid: v() }) }\n"
        )
        self.assertEqual(text.count("ExtensionChange::DropExtension"), 2)
        self.assertEqual(project.rewrites["crates/pbps-cli/src/a.rs"], 2)

    def test_qualified_crate_path_keeps_its_qualifier(self):
        text, _, _ = self.rewrite("fn f(c: &Change) -> bool { matches!(c, pbps_model::Change::DropExtension { .. }) }\n", header="")
        self.assertIn(
            "pbps_model::Change::Extension(pbps_model::ExtensionChange::DropExtension { .. })",
            text,
        )
        self.assertNotIn("use pbps_model::ExtensionChange", text)

    def test_qualified_through_a_module_path(self):
        text, _, _ = self.rewrite("fn f() { let _ = pbps_model::change::Change::DropExtension { uid }; }\n", header="")
        self.assertIn(
            "pbps_model::change::Change::Extension(pbps_model::change::ExtensionChange::DropExtension { uid })",
            text,
        )

    def test_dollar_crate_in_a_macro_body(self):
        text, _, _ = self.rewrite(
            "macro_rules! m { () => { $crate::Change::DropExtension { .. } }; }\n",
            name="crates/pbps-model/src/user.rs",
            header="",
        )
        self.assertIn("$crate::Change::Extension($crate::ExtensionChange::DropExtension { .. })", text)

    def test_the_family_macro_in_change_rs_is_rewritten_too(self):
        self.run_tool()
        text = self.read("crates/pbps-model/src/change.rs")
        self.assertIn("$crate::Change::Extension($crate::ExtensionChange::CreateExtension { .. })", text)
        self.assertIn("$crate::Change::CreateTable { .. }", text)  # other families untouched

    def test_renamed_import_keeps_the_alias_and_imports_the_inner_enum(self):
        text, _, _ = self.rewrite(
            "fn f() -> C { C::DropExtension { uid } }\n", header="use pbps_model::Change as C;\n"
        )
        self.assertIn("C::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertIn("use pbps_model::ExtensionChange;", text)

    def test_type_alias_of_change_is_followed(self):
        text, _, _ = self.rewrite(
            "type C = Change;\nfn f() -> C { C::DropExtension { uid } }\n"
        )
        self.assertIn("C::Extension(ExtensionChange::DropExtension { uid })", text)

    def test_import_in_a_braced_group_gets_its_own_line(self):
        text, _, _ = self.rewrite(
            "fn f() -> Change { Change::DropExtension { uid } }\n",
            header="use pbps_model::{Change, Uid};\n",
        )
        self.assertIn("use pbps_model::{Change, Uid};\nuse pbps_model::ExtensionChange;\n", text)

    def test_import_keeps_its_cfg_attribute(self):
        text, _, _ = self.rewrite(
            "fn f() -> Change { Change::DropExtension { uid } }\n",
            header="#[cfg(test)]\nuse pbps_model::Change;\n",
        )
        self.assertIn("#[cfg(test)]\nuse pbps_model::Change;\n#[cfg(test)]\nuse pbps_model::ExtensionChange;", text)

    def test_glob_import_from_the_model_needs_no_new_import(self):
        text, _, _ = self.rewrite(
            "fn f() -> Change { Change::DropExtension { uid } }\n", header="use pbps_model::*;\n"
        )
        self.assertNotIn("use pbps_model::ExtensionChange", text)
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)

    def test_child_module_through_super_glob_uses_the_parent_import(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\n")
        self.write(
            "crates/pbps-cli/src/a.rs",
            "use pbps_model::Change;\n#[cfg(test)]\nmod tests {\n    use super::*;\n"
            "    fn t() -> Change { Change::DropExtension { uid } }\n}\n",
        )
        self.run_tool()
        text = self.read("crates/pbps-cli/src/a.rs")
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertEqual(text.count("use pbps_model::ExtensionChange;"), 1)

    def test_separate_test_file_through_super_glob(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\n")
        self.write("crates/pbps-cli/src/a.rs", "use pbps_model::Change;\n#[cfg(test)]\nmod tests;\n")
        self.write("crates/pbps-cli/src/a/tests.rs", "use super::*;\nfn t() -> Change { Change::DropExtension { uid } }\n")
        self.run_tool()
        self.assertIn("ExtensionChange::DropExtension", self.read("crates/pbps-cli/src/a/tests.rs"))
        self.assertIn("use super::*;\nuse pbps_model::ExtensionChange;", self.read("crates/pbps-cli/src/a/tests.rs"))
        self.assertNotIn("ExtensionChange", self.read("crates/pbps-cli/src/a.rs"))

    def test_path_attribute_module_resolves_through_the_declaring_file(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\n")
        self.write("crates/pbps-cli/src/a.rs", 'use pbps_model::Change;\n#[cfg(test)]\n#[path = "a_tests.rs"]\nmod tests;\n')
        self.write("crates/pbps-cli/src/a_tests.rs", "use super::*;\nfn t() -> Change { Change::DropExtension { uid } }\n")
        self.run_tool()
        text = self.read("crates/pbps-cli/src/a_tests.rs")
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertIn("use super::*;\nuse pbps_model::ExtensionChange;", text)
        self.assertNotIn("ExtensionChange", self.read("crates/pbps-cli/src/a.rs"))  # no unused import outside tests

    def test_included_file_sees_the_names_of_the_including_module(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\n")
        self.write(
            "crates/pbps-cli/src/a.rs",
            'use pbps_model::Change;\n#[cfg(test)]\nmod tests {\n    use super::*;\n    include!("a_matrix.rs");\n}\n',
        )
        self.write(
            "crates/pbps-cli/src/a_matrix.rs",
            "mod matrix {\n    use super::*;\n    fn t() -> Change { Change::DropExtension { uid } }\n}\n",
        )
        self.run_tool()
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", self.read("crates/pbps-cli/src/a_matrix.rs"))
        self.assertIn("    use super::*;\n    use pbps_model::ExtensionChange;\n", self.read("crates/pbps-cli/src/a_matrix.rs"))

    def test_integration_test_directory_module_resolves_through_its_root(self):
        self.write("crates/pbps-pg/tests/big.rs", "use pbps_model::Change;\nmod part;\n")
        self.write("crates/pbps-pg/tests/big/part.rs", "use super::*;\nfn t() -> Change { Change::DropExtension { uid } }\n")
        self.run_tool()
        self.assertIn("ExtensionChange::DropExtension", self.read("crates/pbps-pg/tests/big/part.rs"))
        self.assertIn("use super::*;\nuse pbps_model::ExtensionChange;", self.read("crates/pbps-pg/tests/big/part.rs"))

    def test_each_function_with_its_own_import_gets_its_own_line(self):
        text, _, _ = self.rewrite(
            "fn a() {\n    use pbps_model::{Change, Uid};\n    let _ = Change::CreateTable { uid };\n}\n"
            "fn b() {\n    use pbps_model::{Change, Uid};\n    let _ = Change::DropExtension { uid };\n}\n",
            header="",
        )
        self.assertEqual(text.count("use pbps_model::ExtensionChange;"), 1)
        self.assertLess(text.index("fn b()"), text.index("use pbps_model::ExtensionChange;"))

    def test_super_import_names_the_inner_enum_by_the_path_the_chain_ends_in(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\n")
        self.write("crates/pbps-cli/src/a.rs", "use pbps_model::Change;\nmod inner {\n    use super::Change;\n    fn t() -> Change { Change::DropExtension { uid } }\n}\n")
        self.run_tool()
        text = self.read("crates/pbps-cli/src/a.rs")
        self.assertIn("use super::Change;\n    use pbps_model::ExtensionChange;", text)
        self.assertNotIn("use pbps_model::Change;\nuse pbps_model::ExtensionChange;", text)  # the parent does not use it

    def test_second_run_changes_nothing(self):
        self.rewrite("fn f(c: &Change) { if let Change::CreateExtension { uid, .. } = c { g(uid) } }\n")
        before = {p: p.read_text() for p in self.root.rglob("*.rs")}
        project, out = self.run_tool()
        self.assertEqual(before, {p: p.read_text() for p in self.root.rglob("*.rs")})
        self.assertEqual(sum(project.rewrites.values()), 0)
        self.assertIn("would rewrite 0 site(s)", out.replace("rewrote", "would rewrite"))

    def test_check_mode_writes_nothing(self):
        path = self.write("crates/pbps-cli/src/a.rs", "use pbps_model::Change;\nfn f() -> Change { Change::DropExtension { uid } }\n")
        before = path.read_text()
        project, out = self.run_tool(write=False)
        self.assertEqual(path.read_text(), before)
        self.assertEqual(project.rewrites["crates/pbps-cli/src/a.rs"], 1)
        self.assertIn("a.rs: would rewrite 1 site(s)", out)

    def test_char_literals_and_lifetimes_do_not_confuse_the_scan(self):
        text, _, _ = self.rewrite(
            "fn f<'a>(s: &'a str, c: char) -> Change {\n    let q = '\\'';\n    let r = '}';\n"
            "    Change::DropExtension { uid: s.len() }\n}\n"
        )
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid: s.len() })", text)


class Negative(Tree):
    def untouched(self, body, header="use pbps_model::Change;\n", name="crates/pbps-cli/src/a.rs"):
        text, project, out = self.rewrite(body, header=header, name=name)
        self.assertEqual(text, header + body)
        return project, out

    def test_another_enum_with_a_variant_of_the_same_name(self):
        self.untouched("fn f() -> Intent { Intent::DropExtension { uid } }\n")

    def test_change_family_names_are_not_changes(self):
        self.untouched("fn f() { let _ = ChangeFamily::Extensions; let _ = Event::DropExtension; }\n")

    def test_a_kind_outside_the_family(self):
        self.untouched("fn f() -> Change { Change::CreateTable { uid } }\n")

    def test_already_rewritten_form(self):
        self.untouched("fn f() -> Change { Change::Extension(ExtensionChange::DropExtension { uid }) }\n")

    def test_comments_and_strings(self):
        project, out = self.untouched(
            '// Change::DropExtension { uid } in a comment\nfn f() -> &\'static str { "Change::DropExtension { uid }" }\n'
            "const R: &str = r#\"Change::DropExtension { x }\"#;\n/* Change::DropExtension { x } */\n"
        )
        self.assertEqual(project.rewrites["crates/pbps-cli/src/a.rs"], 0)

    def test_a_local_change_enum_is_left_alone(self):
        project, _ = self.untouched(
            "enum Change { DropExtension { uid: u32 } }\nfn f() -> Change { Change::DropExtension { uid: 1 } }\n",
            header="",
        )
        self.assertEqual(project.reports, [])

    def test_another_crates_change_import_is_not_the_models(self):
        self.write("crates/pbps-cli/src/lib.rs", "mod a;\nenum Change { DropExtension { uid: u32 } }\n")
        project, _ = self.untouched(
            "fn f() -> Change { Change::DropExtension { uid: 1 } }\n", header="use crate::Change;\n"
        )

    def test_a_second_run_over_rewritten_code_adds_no_import(self):
        self.rewrite("fn f() -> Change { Change::DropExtension { uid } }\n")
        self.run_tool()
        self.assertEqual(self.read("crates/pbps-cli/src/a.rs").count("use pbps_model::ExtensionChange;"), 1)


class Reported(Tree):
    def reasons(self, body, header="use pbps_model::Change;\n", name="crates/pbps-cli/src/a.rs"):
        text, project, out = self.rewrite(body, header=header, name=name)
        return text, project, [r[2] for r in project.reports], header + body

    def test_self_inside_impl_change(self):
        text, _, reasons, original = self.reasons(
            "impl Change {\n    fn f(&self) -> bool { matches!(self, Self::DropExtension { .. }) }\n}\n"
        )
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["`Self::DropExtension` inside `impl Change`"])

    def test_self_inside_a_trait_impl_for_change(self):
        _, _, reasons, _ = self.reasons(
            "impl std::fmt::Display for Change {\n    fn fmt(&self, f: &mut F) -> R { match self { Self::DropExtension { .. } => w(f), _ => v(f) } }\n}\n"
        )
        self.assertEqual(reasons, ["`Self::DropExtension` inside `impl Change`"])

    def test_self_in_another_impl_is_ignored(self):
        _, _, reasons, _ = self.reasons("impl Intent {\n    fn f() -> Self { Self::DropExtension { uid } }\n}\n")
        self.assertEqual(reasons, [])

    def test_glob_import_of_variants_and_the_bare_name_after_it(self):
        _, _, reasons, _ = self.reasons(
            "fn f() -> Change { DropExtension { uid } }\n", header="use pbps_model::Change::*;\n"
        )
        self.assertIn("glob import of Change's variants", reasons)
        self.assertIn("bare `DropExtension` after a variant import", reasons)

    def test_brace_import_of_a_variant(self):
        _, _, reasons, _ = self.reasons(
            "fn f() -> Change { DropExtension { uid } }\n", header="use pbps_model::Change::{self, DropExtension};\n"
        )
        self.assertIn("imports the variant DropExtension", reasons)
        self.assertIn("bare `DropExtension` after a variant import", reasons)

    def test_metavariable_in_variant_position(self):
        _, _, reasons, _ = self.reasons("macro_rules! m { ($k:ident) => { Change::$k { .. } }; }\n")
        self.assertEqual(reasons, ["metavariable in the variant position"])

    def test_macro_rules_body_without_dollar_crate(self):
        text, _, reasons, original = self.reasons(
            "macro_rules! m { () => { Change::DropExtension { .. } }; }\n"
        )
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["inside the macro_rules! body of `m`"])

    def test_unknown_macro_invocation_is_reported_not_guessed(self):
        text, _, reasons, original = self.reasons("fn f() { check!(c, Change::DropExtension { uid }); }\n")
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["inside an invocation of the macro `check!`"])

    def test_matches_arguments_are_rewritten(self):
        text, _, reasons, _ = self.reasons("fn f(c: &Change) -> bool { matches!(c, Change::DropExtension { .. }) }\n")
        self.assertEqual(reasons, [])
        self.assertIn("matches!(c, Change::Extension(ExtensionChange::DropExtension { .. }))", text)

    def test_path_split_across_lines(self):
        text, _, reasons, original = self.reasons("fn f() -> Change {\n    pbps_model::Change\n        ::DropExtension { uid }\n}\n")
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["path is split by whitespace or a comment"])

    def test_kind_used_as_a_function(self):
        text, _, reasons, original = self.reasons("fn f(v: Vec<Box<Thing>>) { v.into_iter().map(Change::RebuildThing); }\n")
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["`RebuildThing` used without its payload group"])

    def test_unrecognised_qualifier(self):
        _, _, reasons, _ = self.reasons("fn f() { let _ = other::Change::DropExtension { uid }; }\n", header="")
        self.assertEqual(reasons, ["unrecognised qualifier `other` before `Change`"])

    def test_unresolvable_change(self):
        _, _, reasons, _ = self.reasons("fn f() -> Change { Change::DropExtension { uid } }\n", header="")
        self.assertEqual(reasons, ["cannot tell which `Change` this is"])

    def test_doc_comment_mention_is_reported_plain_comment_is_not(self):
        text, project, reasons, original = self.reasons(
            "/// See [`Change::DropExtension`].\n// Change::DropExtension is dropped here\nfn f() {}\n"
        )
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["doc comment names a kind of the family"])
        self.assertEqual(project.comment_mentions, 1)

    def test_string_mention_is_a_note(self):
        _, project, _, _ = self.reasons('fn f() -> &\'static str { "Change::DropExtension " }\n')
        self.assertEqual([n[2] for n in project.notes], ["string literal names a kind of the family"])

    def test_unbalanced_file_is_reported_and_left_alone(self):
        text, _, reasons, original = self.reasons("fn f() { Change::DropExtension { uid }\n")
        self.assertEqual(text, original)
        self.assertEqual(reasons, ["file skipped: unbalanced brackets"])

    def test_reports_do_not_stop_the_certain_rewrites(self):
        text, _, reasons, _ = self.reasons(
            "fn f() -> Change { Change::DropExtension { uid } }\nfn g() { check!(Change::DropExtension { uid }); }\n"
        )
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertEqual(reasons, ["inside an invocation of the macro `check!`"])


class MoveEnum(Tree):
    """The step before the rewrite: the variants move into their own enum."""

    def setUp(self):
        super().setUp()
        self.write(
            "crates/pbps-model/src/change.rs",
            CHANGE_RS.replace(
                "    CreateExtension { uid: Uid },\n    DropExtension { uid: Uid },",
                "    /// Creates a package.\n    #[serde(default)]\n    CreateExtension { uid: Uid },\n    DropExtension { uid: Uid },",
            ),
        )
        self.write("crates/pbps-model/src/lib.rs", "pub use change::{\n    Change, ChangeSet,\n};\n")

    def move(self):
        return cfr.move_enum(self.root, "extensions", "Extension", "ExtensionChange")

    def test_the_kinds_move_with_their_docs_and_attributes(self):
        self.assertEqual(self.move(), ["CreateExtension", "DropExtension", "RebuildThing", "Marker"])
        text = self.read("crates/pbps-model/src/change.rs")
        enum = text[text.index("pub enum ExtensionChange"):]
        self.assertIn("    /// Creates a package.\n    #[serde(default)]\n    CreateExtension { uid: Uid },", enum)
        self.assertIn("RebuildThing(Box<Thing>),", enum)
        change = text[text.index("pub enum Change") : text.index("pub enum ExtensionChange")]
        self.assertNotIn("CreateExtension", change)
        self.assertIn("CreateTable { uid: Uid },", change)  # another family stays
        self.assertIn("#[serde(untagged)]\n    Extension(ExtensionChange),", change)

    def test_the_re_export_is_added(self):
        self.move()
        self.assertIn("pub use change::{\n    ExtensionChange,\n    Change,", self.read("crates/pbps-model/src/lib.rs"))

    def test_a_second_move_is_refused_and_changes_nothing(self):
        self.move()
        before = self.read("crates/pbps-model/src/change.rs")
        with self.assertRaises(SystemExit):
            self.move()
        self.assertEqual(self.read("crates/pbps-model/src/change.rs"), before)

    def test_an_unknown_family_is_refused(self):
        with self.assertRaises(SystemExit):
            cfr.move_enum(self.root, "nothing", "N", "NChange")

    def test_the_rewrite_after_the_move_reads_the_kinds_from_the_new_enum(self):
        self.move()
        self.write("crates/pbps-cli/src/a.rs", "use pbps_model::Change;\nfn f() -> Change { Change::DropExtension { uid } }\n")
        self.run_tool()
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", self.read("crates/pbps-cli/src/a.rs"))


class SiteScope(Tree):
    """A fact is used where it is true: an alias, an import or a type alias is
    looked up from the site outwards, and a doubt is reported, not guessed."""

    def test_a_proven_crate_alias_is_followed(self):
        text, _, reasons = self.scoped(
            "use pbps_model as model;\nfn f(c: &model::Change) -> bool { matches!(c, model::Change::DropExtension { .. }) }\n"
        )
        self.assertIn("model::Change::Extension(model::ExtensionChange::DropExtension { .. })", text)
        self.assertEqual(reasons, [])

    def test_the_same_alias_for_another_crate_in_another_scope_is_reported_not_rewritten(self):
        body = (
            "mod mine {\n    use pbps_model as model;\n    pub fn f() -> model::Change { model::Change::DropExtension { uid } }\n}\n"
            "mod theirs {\n    use other_crate as model;\n    pub fn g() -> model::Change { model::Change::DropExtension { uid } }\n}\n"
        )
        text, _, reasons = self.scoped(body)
        self.assertIn("model::Change::Extension(model::ExtensionChange::DropExtension { uid })", text.split("mod theirs")[0])
        self.assertIn("model::Change::DropExtension { uid } }", text.split("mod theirs")[1])
        self.assertEqual(reasons, ["`model` is not the model crate where this is written: it names another item"])

    def test_a_block_that_rebinds_the_alias_shadows_the_outer_one(self):
        body = (
            "use pbps_model as model;\nfn f() {\n    use other_crate as model;\n    let _ = model::Change::DropExtension { uid };\n}\n"
        )
        text, _, reasons = self.scoped(body)
        self.assertNotIn("ExtensionChange", text)
        self.assertEqual(len(reasons), 1)

    def test_a_module_that_is_named_like_the_alias_is_not_the_crate(self):
        text, _, reasons = self.scoped(
            "use pbps_model as model;\nmod inner {\n    mod model {}\n    fn f() { let _ = model::Change::DropExtension { uid }; }\n}\n"
        )
        self.assertNotIn("ExtensionChange", text)
        self.assertEqual(len(reasons), 1)

    def test_an_alias_nothing_binds_is_still_an_unrecognised_qualifier(self):
        _, _, reasons = self.scoped("fn f() { let _ = model::Change::DropExtension { uid }; }\n")
        self.assertEqual(reasons, ["unrecognised qualifier `model` before `Change`"])

    def test_an_existing_import_of_the_models_inner_enum_is_used_not_repeated(self):
        text, _, reasons = self.scoped(
            "use pbps_model::{Change, ExtensionChange};\nfn f() -> Change { Change::DropExtension { uid } }\n"
        )
        self.assertTrue(text.startswith("use pbps_model::{Change, ExtensionChange};\n"))
        self.assertEqual(text.count("use "), 1)
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertEqual(reasons, [])

    def test_an_import_through_a_proven_alias_is_the_models_inner_enum(self):
        text, _, reasons = self.scoped(
            "use pbps_model as model;\nuse model::{Change, ExtensionChange};\nfn f() -> Change { Change::DropExtension { uid } }\n"
        )
        self.assertEqual(text.count("use "), 2)
        self.assertIn("Change::Extension(ExtensionChange::DropExtension { uid })", text)
        self.assertEqual(reasons, [])

    def test_another_crates_enum_of_that_name_is_a_collision_and_nothing_is_rewritten(self):
        body = "use pbps_model::Change;\nuse other::ExtensionChange;\nfn f() -> Change { Change::DropExtension { uid } }\n"
        text, _, reasons = self.scoped(body)
        self.assertEqual(text, body)
        self.assertEqual(reasons, ["`ExtensionChange` is already a different item here (imported or declared); not the model's enum"])

    def test_a_local_enum_of_that_name_is_a_collision(self):
        body = "use pbps_model::Change;\nenum ExtensionChange { A }\nfn f() -> Change { Change::DropExtension { uid } }\n"
        text, _, reasons = self.scoped(body)
        self.assertEqual(text, body)
        self.assertEqual(len(reasons), 1)

    def test_a_collision_in_one_block_leaves_the_sites_of_another_block_alone_to_be_rewritten(self):
        body = (
            "use pbps_model::Change;\nfn a() { use other::ExtensionChange; let _ = Change::DropExtension { uid }; }\n"
            "fn b() { let _ = Change::DropExtension { uid }; }\n"
        )
        text, _, reasons = self.scoped(body)
        self.assertEqual(len(reasons), 1)
        self.assertIn("fn b() { let _ = Change::Extension(ExtensionChange::DropExtension { uid }); }", text)
        self.assertIn("fn a() { use other::ExtensionChange; let _ = Change::DropExtension { uid }; }", text)

    def test_a_qualified_site_needs_no_import_so_a_foreign_one_is_no_collision(self):
        text, _, reasons = self.scoped(
            "use other::ExtensionChange;\nfn f() { let _ = pbps_model::Change::DropExtension { uid }; }\n"
        )
        self.assertIn("pbps_model::Change::Extension(pbps_model::ExtensionChange::DropExtension { uid })", text)
        self.assertEqual(reasons, [])

    def test_a_type_alias_is_the_model_change_only_where_it_is_declared_so(self):
        body = (
            "use pbps_model::Change;\nfn a() { type C = Change; let _ = C::DropExtension { uid }; }\n"
            "fn b() { type C = Elsewhere; let _ = C::DropExtension { uid }; }\n"
        )
        text, _, reasons = self.scoped(body)
        self.assertIn("type C = Change; let _ = C::Extension(ExtensionChange::DropExtension { uid });", text)
        self.assertIn("type C = Elsewhere; let _ = C::DropExtension { uid };", text)
        self.assertEqual(reasons, [])

    def test_a_macro_this_file_defines_does_not_read_as_the_std_one(self):
        body = "use pbps_model::Change;\nmacro_rules! matches { ($e:expr, $p:pat) => { false }; }\nfn f(c: &Change) -> bool { matches!(c, Change::DropExtension { .. }) }\n"
        text, _, reasons = self.scoped(body)
        self.assertEqual(text, body)
        self.assertEqual(reasons, ["inside an invocation of the macro `matches!`"])

    def scoped(self, body, name="crates/pbps-cli/src/a.rs"):
        self.write(name, body)
        _, out = self.run_tool()[0], None
        project = self.last_project
        return self.read(name), project, [r[2] for r in project.reports if r[0] == name]


class Lexer(unittest.TestCase):
    def texts(self, src):
        return [t.text for t in cfr.lex(src)[0]]

    def test_lifetime_is_not_a_char(self):
        self.assertEqual(self.texts("&'a T"), ["&", "'a", "T"])

    def test_char_literals(self):
        self.assertEqual(self.texts("'}' '\\'' '\\n' 'x'"), ["'}'", "'\\''", "'\\n'", "'x'"])

    def test_raw_and_byte_strings_hide_their_contents(self):
        self.assertEqual(len(self.texts('r##"a "# b"## b"x\\"y" br#"z"#')), 3)

    def test_nested_block_comment(self):
        toks, comments = cfr.lex("a /* x /* y */ z */ b")
        self.assertEqual([t.text for t in toks], ["a", "b"])
        self.assertEqual(len(comments), 1)

    def test_doc_comment_kinds(self):
        _, comments = cfr.lex("/// d\n//// not\n//! d\n/** d */\n/*** not */\n// plain\n")
        self.assertEqual([c.doc for c in comments], [True, False, True, True, False, False])

    def test_unterminated_string_is_an_error(self):
        with self.assertRaises(cfr.LexError):
            cfr.lex('"abc')


if __name__ == "__main__":
    unittest.main()
