#!/usr/bin/env python3
"""Tests for scripts/check-raw-in-messages.py. Run: python3 scripts/check_raw_in_messages_test.py"""

import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_raw_in_messages", Path(__file__).with_name("check-raw-in-messages.py")
)
guard = importlib.util.module_from_spec(spec)
spec.loader.exec_module(guard)

APP = "crates/pbps-cli/src/app.rs"
BUILDER = "crates/pbps-pg/src/emit.rs"
BUILDERS = {BUILDER: (None, "builds statements")}

BAIL = 'fn f(t: &Table) -> Result<()> {\n    bail!("no such table {}", t.name.raw());\n}\n'


def run(text, path=APP, allowed=None):
    allowed = BUILDERS if allowed is None else allowed
    # Every entry names a file that exists; the rest of the tree is empty here.
    files = {p: "" for p in allowed}
    files[path] = text
    return guard.check(files, allowed)


def lines(text):
    return [n for n, _ in guard.hits(text)[0]]


class WhatItRefuses(unittest.TestCase):
    def test_raw_in_a_bail_is_refused_with_its_line(self):
        (error,) = run(BAIL)
        self.assertTrue(error.startswith(f"{APP}:2: `.raw()` inside bail!"), error)

    def test_every_listed_macro_is_refused(self):
        for macro in ["format", "bail", "anyhow", "print", "println", "eprint", "eprintln"]:
            self.assertEqual(lines(f'{macro}!("{{}}", n.raw());'), [1], macro)
        for macro in ["write", "writeln"]:
            self.assertEqual(lines(f'{macro}!(out, "{{}}", n.raw());'), [1], macro)

    def test_ensure_builds_a_message_too(self):
        self.assertEqual(lines('ensure!(ok, "bad {}", n.raw());'), [1])
        self.assertEqual(lines('anyhow::ensure!(ok, "bad {}", n.raw());'), [1])
        text = 'ensure!(\n    a.raw() == b.raw(),\n    "bad {}",\n    n.raw(),\n);'
        self.assertEqual(lines(text), [4])

    def test_the_condition_of_an_ensure_is_not_a_message(self):
        for text in [
            'ensure!(a.raw() == b.raw(), "bad {}", n.shown());',
            'ensure!(same(a.raw(), b), "bad {}", n.shown());',
            'ensure!(a.raw() == b.raw())',
        ]:
            self.assertEqual(lines(text), [], text)

    def test_every_delimiter_of_a_macro_call_is_read(self):
        for call in ['format!["{}", n.raw()]', 'format!{"{}", n.raw()}']:
            self.assertEqual(lines(call), [1], call)

    def test_raw_in_a_thiserror_message_is_refused(self):
        text = '#[derive(Error)]\nenum E {\n    #[error("no {} here", .0.raw())]\n    A(Name),\n}\n'
        self.assertEqual(lines(text), [3])

    def test_a_multi_line_call_is_read_to_its_end(self):
        text = (
            "fn f() {\n"
            "    bail!(\n"
            '        "{} and {}",\n'
            "        a.shown(),\n"
            "        b\n"
            "            .raw(),\n"
            "    );\n"
            "}\n"
        )
        self.assertEqual(lines(text), [6])

    def test_a_raw_after_a_multi_line_call_is_not_part_of_it(self):
        text = 'bail!(\n    "{}",\n    a.shown(),\n);\nlet k = b.raw();\n'
        self.assertEqual(lines(text), [])

    def test_a_raw_in_a_nested_macro_is_one_finding(self):
        self.assertEqual(lines('bail!("{}", format!("{}", n.raw()));'), [1])

    def test_a_raw_in_a_closure_in_the_arguments_is_refused(self):
        text = 'format!("{}", cols.iter().map(|c| c.raw()).collect::<Vec<_>>().join(","))'
        self.assertEqual(lines(text), [1])

    def test_the_function_passed_on_is_refused_too(self):
        self.assertEqual(lines('format!("{:?}", d.as_ref().map(Text::raw))'), [1])

    def test_a_spelling_rustfmt_keeps_is_still_the_call(self):
        # A comment or a line break between the pieces is whitespace once
        # comments are blanked; a raw identifier is the same name.
        for call in [
            'println!("{}", text. /* original bytes */ raw())',
            'println!("{}", Text:: /* method */ raw(&text))',
            'println!("{}", text\n    .\n    raw())',
            'println!("{}", text.r#raw())',
            'println!("{:?}", d.map(Text::r#raw))',
            'println!("{}", text.raw ())',
        ]:
            self.assertEqual(len(lines(call)), 1, call)

    def test_a_macro_spelled_with_spaces_or_a_comment_is_still_the_macro(self):
        for call in [
            'format /* m */ ! /* d */ ("{}", n.raw())',
            'std::format ! ("{}", n.raw())',
            '# [ error ( "no {}", .0.raw() ) ]',
            '#[ /* a */ error /* b */ ("no {}", .0.raw())]',
        ]:
            self.assertEqual(len(lines(call)), 1, call)

    def test_each_finding_names_its_own_line(self):
        text = 'format!("{}", a.raw());\nlet ok = b.raw();\nbail!("{}", c.raw());\n'
        self.assertEqual(lines(text), [1, 3])

    def test_a_file_that_is_not_allowed_is_refused_where_a_builder_is_not(self):
        text = 'let sql = format!("alter table {}", quote(t.name.raw()));\n'
        self.assertEqual(len(run(text)), 1)
        self.assertEqual(run(text, path=BUILDER), [])


class WhatItAccepts(unittest.TestCase):
    def test_the_same_call_in_an_allowlisted_builder_is_accepted(self):
        self.assertEqual(run(BAIL, path=BUILDER), [])

    def test_the_real_allowlist_accepts_it_in_each_builder_it_names(self):
        unbounded = {p: v for p, v in guard.ALLOWED.items() if v[0] is None}
        self.assertTrue(unbounded)
        self.assertEqual(guard.check({p: BAIL for p in unbounded}, unbounded), [])

    def test_the_real_allowlist_refuses_it_anywhere_else(self):
        elsewhere = "crates/pbps-cli/src/deploy.rs"
        self.assertNotIn(elsewhere, guard.ALLOWED)
        self.assertEqual(len(guard.check({elsewhere: BAIL}, {})), 1)

    def test_raw_outside_a_message_is_accepted(self):
        for text in [
            "let rows = conn.query(sql, &[&name.raw()]).await?;",
            "map.insert(column.name.raw().to_owned(), column);",
            "let same = a.description.raw() == b.description.raw();",
            "let d = d.as_ref().map(Text::raw);",
            "vec![a.raw(), b.raw()]",
            'assert_eq!(a.raw(), "x", "{}", b.shown());',
            "let s = S { a: 1, ..raw };",
        ]:
            self.assertEqual(run(text), [], text)

    def test_shown_in_a_message_is_accepted(self):
        for text in [
            'bail!("no such table {}", t.name.shown());',
            'format!("{}", Text::new(prefix).shown())',
            '#[error("no {} here", .0.shown())]',
            '#[error("no {0} here")]',
        ]:
            self.assertEqual(run(text), [], text)

    def test_another_macro_is_not_a_message(self):
        for text in ['my_format!("{}", n.raw());', 'sql!("{}", n.raw());', 'matches!(n.raw(), "a")']:
            self.assertEqual(run(text), [], text)

    def test_a_struct_update_in_a_message_is_not_a_raw_call(self):
        self.assertEqual(lines('format!("{:?}", S { a: 1, ..raw })'), [])


class WhatItDoesNotMistake(unittest.TestCase):
    def test_a_string_or_a_comment_is_not_code(self):
        for text in [
            'bail!("call .raw() on nothing here: {}", n.shown());',
            'bail!("{}", n.shown()); // never n.raw()\n',
            'bail!("{}", n.shown()); /* format!("{}", n.raw()) /* nested */ */\n',
            '// bail!("{}", n.raw());\nlet x = 1;\n',
            '/// ```\n/// bail!("{}", n.raw());\n/// ```\nfn f() {}\n',
        ]:
            self.assertEqual(lines(text), [], text)

    def test_a_delimiter_in_a_string_does_not_end_the_call(self):
        self.assertEqual(lines('format!("a) ] }} {}", n.raw())'), [1])
        self.assertEqual(lines('format!("{}", ")"); n.raw()'), [])

    def test_a_delimiter_in_a_character_does_not_end_the_call(self):
        self.assertEqual(lines("format!(\"{}\", ')'); n.raw()"), [])
        self.assertEqual(lines("format!(\"{}{}\", '(', n.raw())"), [1])
        self.assertEqual(lines("bail!(\"{}\", '\"'); n.raw()"), [])
        self.assertEqual(lines("bail!(\"{}\", '\\''); n.raw()"), [])

    def test_a_lifetime_does_not_open_a_character(self):
        self.assertEqual(lines('format!("{}", f::<&\'static str>(n.raw()))'), [1])
        self.assertEqual(lines("bail!(\"{}\", g::<'a>(x)); n.raw()"), [])

    def test_a_raw_string_is_read_to_its_own_end(self):
        self.assertEqual(lines('println!(r#"a ) " b"#, n.shown()); n.raw()'), [])
        self.assertEqual(lines('println!(r#"a"#, n.raw())'), [1])
        self.assertEqual(lines('println!(br"a )", n.raw())'), [1])

    def test_an_unterminated_macro_call_is_an_error_not_a_pass(self):
        errors = run('fn f() {\n    bail!("{}", n.shown()\n}\n')
        self.assertEqual(len(errors), 1)
        self.assertIn(f"{APP}:2: cannot find the end", errors[0])
        # Not even in a file that is allowed to hold raw texts.
        self.assertEqual(len(run('bail!("{}"', path=BUILDER)), 1)


class TheAllowlist(unittest.TestCase):
    HOLE = {APP: (1, "a hole")}

    def test_a_known_hole_holds_exactly_its_count(self):
        self.assertEqual(run('format!("{}", n.raw())', allowed=self.HOLE), [])

    def test_a_new_one_beside_a_known_hole_is_refused(self):
        text = 'format!("{}", a.raw()); bail!("{}", b.raw());'
        (error,) = run(text, allowed=self.HOLE)
        self.assertIn("expects 1", error)
        self.assertIn("there are 2", error)

    def test_a_fixed_hole_must_lower_the_count(self):
        (error,) = run('bail!("{}", n.shown())', allowed=self.HOLE)
        self.assertIn("there are 0", error)

    def test_an_entry_for_a_file_that_is_gone_is_refused(self):
        errors = guard.check({}, BUILDERS)
        self.assertEqual(len(errors), 1)
        self.assertIn("no such file", errors[0])

    def test_an_entry_without_a_reason_or_with_a_bad_count_is_refused(self):
        for entry in [(None, ""), (None, "  "), (0, "why"), ("1", "why"), (-1, "why")]:
            errors = guard.check({APP: "let x = 1;"}, {APP: entry})
            self.assertEqual(len(errors), 1, entry)

    def test_the_real_allowlist_says_why_for_every_entry(self):
        for path, (count, why) in guard.ALLOWED.items():
            self.assertTrue(why.strip(), path)
            self.assertTrue(path.startswith(guard.SCANNED) and path.endswith(".rs"), path)
            self.assertTrue(count is None or count >= 1, path)

    def test_the_known_holes_cite_the_step_that_removes_them(self):
        holes = {p: v for p, v in guard.ALLOWED.items() if v[0] is not None}
        self.assertEqual(
            sorted(holes), ["crates/pbps-cli/src/report.rs", "crates/pbps-cli/src/status.rs"]
        )
        for path, (_, why) in holes.items():
            self.assertIn("#2084 step 5", why, path)


if __name__ == "__main__":
    unittest.main()
