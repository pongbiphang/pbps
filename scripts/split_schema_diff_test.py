#!/usr/bin/env python3
"""Tests for split_schema_diff.py: the lexer, the move, and the move check.

Every check that must refuse something has a case that makes it refuse. The
trees are small Rust-shaped fixtures in a temporary directory; nothing here
needs cargo or rustfmt.
"""

from __future__ import annotations

import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import split_schema_diff as ssd  # noqa: E402

REL = "crates/pbps-diff/src/schema_diff.rs"

PARENT = '''//! Fixture: a parent file the script cuts.

use std::collections::BTreeMap;

/// Why this is an error:
///
/// ```
/// let s = "{";
/// fn not_an_item() {
/// ```
#[derive(Debug)]
pub enum Boom {
    One { reason: String },
}

/// Trims.
fn alpha<'a>(x: &'a str, open: char) -> &'a str {
    // a comment with a brace }
    let _s = "}{";
    let _r = r##"} "# {"##;
    let _c = '{';
    /* nested /* block { */ still comment { */
    x
}

struct Pair {
    left: u8,
    right: u8,
}

impl Pair {
    fn sum(&self) -> u8 {
        self.left + self.right
    }
}

fn beta(a: u8, b: u8) -> u8 {
    let t = (a, b);
    t.0 + alpha("x", '{').len() as u8
}

const LIMIT: usize = 3;

fn gamma() -> Pair {
    Pair { left: 1, right: 2 }
}

type Alias = BTreeMap<u8, u8>;

#[cfg(test)]
mod tests;
'''

TESTS = "use super::*;\n\n#[test]\nfn limit_is_three() {\n    assert_eq!(LIMIT, 3);\n}\n"

PLAN = {
    "_docs": {"first": "The first module.", "second": "The second module."},
    "_pub_super": ["Pair.left", "Pair::sum"],
    "first": ["Boom", "alpha"],
    "second": ["Pair", "LIMIT"],
}


def git(root: Path, *args: str) -> None:
    subprocess.run(
        ["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", "-C", str(root), *args],
        check=True,
        capture_output=True,
    )


class Tree(unittest.TestCase):
    """A fixture tree with the plan applied, and the base copy kept."""

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="split-test-"))
        self.addCleanup(shutil.rmtree, self.tmp, True)
        self.root = self.tmp / "tree"
        self.src = self.root / REL
        self.dir = self.src.with_suffix("")
        self.dir.mkdir(parents=True)
        self.src.write_text(PARENT)
        (self.dir / "tests.rs").write_text(TESTS)
        self.plan = self.tmp / "plan.json"
        self.write_plan(PLAN)
        self.base = PARENT
        git(self.root, "init", "-q")
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "base")

    def write_plan(self, plan: dict) -> None:
        self.plan.write_text(json.dumps(plan))

    def apply(self) -> list[str]:
        return ssd.apply(self.root, REL, self.plan)

    def check(self, base: str | None = None, ref: str | None = "HEAD") -> list[str]:
        return ssd.check(self.root, REL, self.base if base is None else base, ref)[0]

    def module(self, name: str) -> Path:
        return self.dir / f"{name}.rs"

    def edit(self, path: Path, old: str, new: str) -> None:
        text = path.read_text()
        self.assertEqual(text.count(old), 1, f"{old!r} must occur once in {path.name}")
        path.write_text(text.replace(old, new))

    def item_text(self, path: Path, name: str) -> str:
        return next(i.source for i in ssd.cut_items(path.read_text()) if i.name == name)

    def swap(self, path: Path, a: str, b: str) -> None:
        text = path.read_text()
        items = {i.name: i for i in ssd.cut_items(text) if i.name in (a, b)}
        first, second = sorted(items.values(), key=lambda i: i.start)
        path.write_text(
            text[: first.start]
            + second.source
            + text[first.stop : second.start]
            + first.source
            + text[second.stop :]
        )

    def assertRefused(self, problems: list[str], *words: str) -> None:
        self.assertTrue(problems, "the check passed but must have failed")
        for w in words:
            self.assertTrue(any(w in p for p in problems), f"no problem mentions {w!r}: {problems}")


class Move(Tree):
    def test_the_plan_moves_items_in_source_order_and_the_check_passes(self) -> None:
        self.apply()
        first = self.module("first").read_text()
        self.assertTrue(first.startswith("//! The first module.\n\nuse super::*;\n\n"))
        self.assertLess(first.index("pub enum Boom"), first.index("fn alpha"))
        self.assertEqual(self.check(), [])
        parent = self.src.read_text()
        self.assertNotIn("fn alpha", parent)
        self.assertIn("fn beta", parent)

    def test_the_parent_gains_mod_lines_and_named_imports_only(self) -> None:
        self.apply()
        parent = self.src.read_text()
        self.assertIn("mod first;\nmod second;\n", parent)
        self.assertIn("pub use first::Boom;", parent)
        self.assertIn("use first::alpha;", parent)
        self.assertNotIn("::*;", parent)

    def test_visibility_is_inserted_only_where_a_name_is_used_outside_the_module(self) -> None:
        self.apply()
        self.assertIn("pub(super) fn alpha", self.module("first").read_text())  # beta uses it
        self.assertIn("pub(super) const LIMIT", self.module("second").read_text())  # a test file uses it
        self.assertIn("pub enum Boom", self.module("first").read_text())  # keeps `pub`
        self.assertNotIn("pub(super) pub", self.module("first").read_text())

    def test_a_private_item_nobody_outside_uses_stays_private(self) -> None:
        self.write_plan({**PLAN, "second": ["Pair"], "first": ["Boom", "alpha"], "_docs": PLAN["_docs"]})
        self.src.write_text(PARENT.replace("alpha(\"x\", '{')", "1"))
        self.base = self.src.read_text()
        self.apply()
        self.assertNotIn("pub(super) fn alpha", self.module("first").read_text())
        self.assertNotIn("use first::alpha;", self.src.read_text())

    def test_listed_fields_and_methods_get_pub_super(self) -> None:
        self.apply()
        second = self.module("second").read_text()
        self.assertIn("    pub(super) left: u8,", second)
        self.assertIn("    right: u8,", second)
        self.assertIn("    pub(super) fn sum", second)

    def test_a_second_run_changes_nothing(self) -> None:
        self.apply()
        before = {p: p.read_bytes() for p in [self.src, *self.dir.glob("*.rs")]}
        self.assertEqual(self.apply(), [])
        self.assertEqual(before, {p: p.read_bytes() for p in [self.src, *self.dir.glob("*.rs")]})

    def test_a_later_plan_adds_a_module_and_appends_to_an_existing_one(self) -> None:
        self.apply()
        git(self.root, "add", "-A")
        git(self.root, "commit", "-q", "-m", "first move")
        base = self.src.read_text()
        later = {**PLAN, "_docs": {**PLAN["_docs"], "third": "The third."}}
        later["first"] = ["Boom", "alpha", "Alias"]
        later["third"] = ["gamma"]
        self.write_plan(later)
        self.apply()
        self.assertTrue(self.module("third").is_file())
        self.assertIn("type Alias", self.module("first").read_text())
        self.assertEqual(self.check(base, "HEAD"), [])

    def test_a_name_the_tree_does_not_have_is_an_error_not_a_skip(self) -> None:
        self.write_plan({**PLAN, "second": ["Pair", "NOPE"]})
        with self.assertRaisesRegex(ssd.SplitError, "NOPE"):
            self.apply()
        self.assertFalse(self.module("first").exists(), "nothing is written when the run fails")

    def test_a_name_listed_twice_or_a_module_without_a_doc_is_an_error(self) -> None:
        self.write_plan({**PLAN, "second": ["Pair", "alpha"]})
        with self.assertRaisesRegex(ssd.SplitError, "alpha"):
            self.apply()
        self.write_plan({"first": ["Boom"]})
        with self.assertRaisesRegex(ssd.SplitError, "_docs"):
            self.apply()

    def test_the_map_names_the_old_lines_and_the_module(self) -> None:
        rows = ssd.move_map(PARENT, self.plan)
        self.assertIn("Boom: 5-14 -> first", rows)
        self.assertIn("impl Pair: 31-35 -> second", rows)
        self.assertEqual(len(rows), 5)

    def test_it_reads_the_tree_and_no_line_numbers(self) -> None:
        # The same items at other places, with other lines between them.
        text = PARENT.replace("const LIMIT: usize = 3;\n\n", "")
        text = text.replace("/// Trims.", "fn filler() {}\n\n" * 30 + "const LIMIT: usize = 3;\n\n/// Trims.")
        self.src.write_text(text)
        self.base = text
        self.apply()
        self.assertEqual(self.check(), [])
        self.assertIn("pub(super) const LIMIT", self.module("second").read_text())


class Refusals(Tree):
    """Each way a move can go wrong must make the check fail."""

    def setUp(self) -> None:
        super().setUp()
        self.apply()

    def test_a_token_changed_inside_a_moved_item(self) -> None:
        self.edit(self.module("second"), "self.left + self.right", "self.left - self.right")
        self.assertRefused(self.check(), "differs from the base", "impl Pair")

    def test_an_item_present_twice(self) -> None:
        self.src.write_text(self.src.read_text() + "\n" + self.item_text(self.module("first"), "alpha") + "\n")
        self.assertRefused(self.check(), "present 2 times", "alpha")

    def test_an_item_missing(self) -> None:
        self.edit(self.module("second"), self.item_text(self.module("second"), "LIMIT"), "")
        self.assertRefused(self.check(), "LIMIT", "missing")

    def test_a_changed_order_within_a_module(self) -> None:
        self.swap(self.module("second"), "Pair", "LIMIT")
        self.assertRefused(self.check(), "order changed in module `second`")

    def test_a_changed_order_in_the_parent(self) -> None:
        self.swap(self.src, "beta", "gamma")
        self.assertRefused(self.check(), "order changed in the parent")

    def test_an_extra_item(self) -> None:
        self.module("first").write_text(self.module("first").read_text() + "\nfn extra() {}\n")
        self.assertRefused(self.check(), "extra item", "extra")

    def test_a_pub_super_inserted_inside_a_body(self) -> None:
        self.edit(self.module("second"), "self.left + self.right", "self.left + pub(super) self.right")
        self.assertRefused(self.check(), "differs from the base")

    def test_a_pub_super_in_place_of_another_visibility(self) -> None:
        self.edit(self.module("first"), "pub enum Boom", "pub(super) enum Boom")
        self.assertRefused(self.check(), "Boom")

    def test_an_item_moved_to_the_wrong_module_is_still_one_item_but_the_order_check_sees_it(self) -> None:
        # alpha cut out of `first` and put after LIMIT in `second`: no longer where the plan left it,
        # so `first` lost an item the base had once and `second` order is not source order.
        alpha = self.item_text(self.module("first"), "alpha")
        self.edit(self.module("first"), alpha, "")
        self.module("second").write_text(self.module("second").read_text() + "\n" + alpha + "\n")
        self.assertRefused(self.check(), "order changed in module `second`")

    def test_a_removed_use_or_mod_line(self) -> None:
        self.edit(self.src, "use std::collections::BTreeMap;\n", "")
        self.assertRefused(self.check(), "was removed")

    def test_a_changed_test_declaration(self) -> None:
        self.edit(self.src, "#[cfg(test)]\nmod tests;", "mod tests;")
        self.assertRefused(self.check(), "test module declarations changed")

    def test_a_changed_test_file(self) -> None:
        (self.dir / "tests.rs").write_text(TESTS + "\n// edited\n")
        self.assertRefused(self.check(), "tests.rs differs from HEAD")

    def test_a_declared_module_without_a_file(self) -> None:
        self.module("second").unlink()
        self.assertRefused(self.check(), "has no file")

    def test_an_unbalanced_file_is_an_error_not_a_pass(self) -> None:
        self.module("first").write_text(self.module("first").read_text() + "\nfn broken() {\n")
        with self.assertRaises(ssd.SplitError):
            self.check()


class Allowed(Tree):
    """What the check must let through."""

    def setUp(self) -> None:
        super().setUp()
        self.apply()

    def test_a_re_wrapped_signature_gains_a_trailing_comma(self) -> None:
        self.edit(
            self.module("first"),
            "fn alpha<'a>(x: &'a str, open: char) -> &'a str {",
            "fn alpha<'a>(\n    x: &'a str,\n    open: char,\n) -> &'a str {",
        )
        self.assertEqual(self.check(), [])

    def test_a_trailing_comma_anywhere_else_is_refused(self) -> None:
        # `(a, b)` and `(a, b,)` are the same pair, but `(a)` and `(a,)` are not the same type:
        # only a parameter list may gain the comma.
        self.edit(self.src, "let t = (a, b);", "let t = (a, b,);")
        self.assertRefused(self.check(), "beta")

    def test_a_comma_before_a_closing_paren_in_a_call_is_refused(self) -> None:
        self.edit(self.src, "alpha(\"x\", '{')", "alpha(\"x\", '{',)")
        self.assertRefused(self.check(), "beta")

    def test_a_blank_line_or_a_new_layout_is_not_a_difference(self) -> None:
        self.edit(self.module("second"), "pub(super) const LIMIT: usize = 3;", "pub(super) const LIMIT: usize =\n    3;")
        self.assertEqual(self.check(), [])

    def test_a_comment_changed_is_a_difference(self) -> None:
        self.edit(self.module("first"), "// a comment with a brace }", "// a comment with a brace )")
        self.assertRefused(self.check(), "alpha")


class Lexer(unittest.TestCase):
    def items(self, text: str) -> list[ssd.Item]:
        return [i for i in ssd.cut_items(text) if not i.glue]

    def test_braces_inside_a_string_do_not_close_the_item(self) -> None:
        got = self.items('fn a() { let s = "}}}"; }\nfn b() {}\n')
        self.assertEqual([i.name for i in got], ["a", "b"])

    def test_a_string_with_an_escaped_quote(self) -> None:
        got = self.items('fn a() { let s = "\\"}"; }\nfn b() {}\n')
        self.assertEqual([i.name for i in got], ["a", "b"])

    def test_raw_strings_with_hashes(self) -> None:
        text = 'fn a() { let s = r##"} "# }"##; let t = br#"{"#; }\nfn b() {}\n'
        self.assertEqual([i.name for i in self.items(text)], ["a", "b"])

    def test_char_literals_are_not_braces_and_lifetimes_are_not_chars(self) -> None:
        text = "fn a<'a>(x: &'a str) -> &'a str { let c = '{'; let d = '}'; let e = '\\''; let f = '\\\\'; x }\nfn b() {}\n"
        got = self.items(text)
        self.assertEqual([i.name for i in got], ["a", "b"])
        kinds = [t.kind for t in ssd.tokenize("&'a str '{'")]
        self.assertEqual(kinds, ["p", "life", "id", "str"])

    def test_a_lifetime_followed_by_a_brace_is_not_a_char(self) -> None:
        got = self.items("impl<'a> X<'a> { fn f(&self) {} }\nfn b() {}\n")
        self.assertEqual([i.name for i in got], ["X", "b"])

    def test_nested_block_comments(self) -> None:
        text = "fn a() { /* x /* { */ } */ }\nfn b() {}\n"
        self.assertEqual([i.name for i in self.items(text)], ["a", "b"])
        with self.assertRaises(ssd.SplitError):
            ssd.tokenize("/* /* */")

    def test_doc_comments_with_code_fences_hold_code_that_is_not_code(self) -> None:
        text = "/// ```\n/// fn fake() {\n/// ```\nfn real() {}\n//// not a doc\nfn next() {}\n"
        got = self.items(text)
        self.assertEqual([i.name for i in got], ["real", "next"])
        self.assertEqual([t.kind for t in ssd.tokenize(text)][:3], ["doc", "doc", "doc"])
        self.assertEqual(ssd.tokenize("//// x")[0].kind, "com")

    def test_leading_comments_and_attributes_belong_to_the_item(self) -> None:
        text = "// one\n/// two\n#[derive(Debug)]\n#[allow(x)]\npub struct S;\n\nfn g() {}\n"
        first = self.items(text)[0]
        self.assertTrue(first.source.startswith("// one"))
        self.assertTrue(first.source.endswith("struct S;"))
        self.assertEqual((first.kind, first.name, first.vis), ("struct", "S", "pub"))

    def test_how_an_item_is_named(self) -> None:
        text = (
            "pub(crate) fn f() {}\nstruct S { a: u8 }\nstruct T(u8);\nenum E { A }\nconst C: u8 = { 1 };\n"
            "static mut G: u8 = 0;\ntype Y = Vec<u8>;\nimpl S {}\nimpl<'a> T for S {}\nimpl fmt::Display for E {}\n"
            "impl<T: Fn() -> u8> Wrapper<T> {}\nconst fn k() {}\nunsafe fn u() {}\n"
        )
        got = [(i.kind, i.name, i.display) for i in self.items(text)]
        self.assertEqual(
            got,
            [
                ("fn", "f", ""),
                ("struct", "S", ""),
                ("struct", "T", ""),
                ("enum", "E", ""),
                ("const", "C", ""),
                ("static", "G", ""),
                ("type", "Y", ""),
                ("impl", "S", "impl S"),
                ("impl", "S", "impl T for S"),
                ("impl", "E", "impl Display for E"),
                ("impl", "Wrapper", "impl Wrapper"),
                ("fn", "k", ""),
                ("fn", "u", ""),
            ],
        )

    def test_a_const_whose_value_holds_braces_and_semicolons_is_one_item(self) -> None:
        text = "const A: [u8; 2] = { let x = 1; [x, 2] };\nfn b() {}\n"
        self.assertEqual([i.name for i in self.items(text)], ["A", "b"])

    def test_use_and_mod_lines_are_glue_not_items(self) -> None:
        text = "use a::b;\npub use c::{d, e};\n#[cfg(test)]\nmod tests;\nfn f() {}\n"
        its = ssd.cut_items(text)
        self.assertEqual([i.kind for i in its], ["use", "use", "mod_decl", "fn"])
        self.assertTrue(its[2].is_test_gated())
        self.assertTrue(its[0].glue and not its[3].glue)

    def test_a_struct_field_or_method_head_is_found_after_attributes_and_docs(self) -> None:
        text = "struct S {\n    /// d\n    #[x]\n    a: u8,\n    pub b: (u8, u8),\n}\nimpl S {\n    /// d\n    fn m(&self) {}\n    pub fn n() {}\n}\n"
        s, i = self.items(text)
        self.assertEqual([(m[0], m[2]) for m in ssd.members(s)], [("a", False), ("b", True)])
        self.assertEqual([(m[0], m[2]) for m in ssd.members(i)], [("m", False), ("n", True)])


class Cli(Tree):
    def run_script(self, *args: str) -> subprocess.CompletedProcess:
        script = Path(ssd.__file__)
        return subprocess.run(
            [sys.executable, str(script), "--root", str(self.root), "--plan", str(self.plan), *args],
            capture_output=True,
            text=True,
        )

    def test_the_exit_codes(self) -> None:
        saved = self.tmp / "base.rs"
        saved.write_text(PARENT)
        self.assertEqual(self.run_script().returncode, 0)
        ok = self.run_script("--check", str(saved), "--base-ref", "HEAD")
        self.assertEqual(ok.returncode, 0, ok.stdout)
        self.assertIn("problems: 0", ok.stdout)
        self.assertIn("moved: first 2, second 3", ok.stdout)
        self.edit(self.module("second"), "self.left + self.right", "self.left * self.right")
        bad = self.run_script("--check", str(saved), "--base-ref", "HEAD")
        self.assertEqual(bad.returncode, 1)
        self.assertIn("PROBLEM:", bad.stdout)
        self.write_plan({**PLAN, "first": ["Boom", "NOPE"]})
        self.assertEqual(self.run_script().returncode, 2)


if __name__ == "__main__":
    unittest.main()
