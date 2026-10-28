"""Negative fixtures for the acceptance-table contract, independent of Cargo."""
from pathlib import Path
import json
import tempfile
import unittest

import check_resolver_acceptance as acceptance


class AcceptanceTable(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        for directory in ("docs", "scripts", "crates/example/src"):
            (self.root / directory).mkdir(parents=True)
        (self.root / "docs/ADR-0016-engine-assisted-planning.md").write_text(
            "**Acceptance tests for the resolver (live, PostgreSQL):**\n- " + chr(96) + "proof" + chr(96) +
            "\n## Architectural placement\n")
        self.source = self.root / "crates/example/src/lib.rs"
        self.source.write_text('#[tokio::test]\n#[ignore = "private fixture"]\nasync fn proof() {}\n')
        self.inventory = {
            "owners": {"pg": {"job": "live-pg", "platform": "linux"},
                       "quick": {"job": "quick", "platform": "linux", "ordinary": True}},
            "groups": [{"target": ["example", "lib", "example"], "owner": "pg", "cases": ["proof"]}],
        }
        self.cases = [[str(i), "tests", "proof", "ADR-0016"] for i in range(1, 25)]
        self.tests = [["proof", "proof", "crates/example/src/lib.rs", "example/lib/example",
                       "pg", "live-pg", "linux", "PG 16/18", "server", "pending closing audit"]]

    def check(self):
        def table(header, rows):
            return "\n".join("| " + " | ".join(row) + " |"
                             for row in [header, ["---"] * len(header), *rows])
        document = ("<!-- acceptance-cases -->\n" +
                    table(acceptance.CASE_HEADER, [row + ["per-test", "per-test", "per-test"] for row in self.cases]) +
                    "\n\n<!-- acceptance-tests -->\n" +
                    table(acceptance.TEST_HEADER, [row + ["PR fixture"] for row in self.tests]) + "\n")
        (self.root / "docs/RESOLVER-ACCEPTANCE.md").write_text(document)
        (self.root / "scripts/ignored-test-owners.json").write_text(json.dumps(self.inventory))
        return acceptance.check(self.root)

    def test_registered_mapping_is_not_execution_evidence(self):
        self.assertEqual(self.check(), (24, 1))

    def test_missing_amendment_mapping_refuses(self):
        p = self.root / "docs/ADR-0016-engine-assisted-planning.md"
        p.write_text(p.read_text().replace("## Architectural placement",
                     "- " + chr(96) + "another_proof" + chr(96) + "\n## Architectural placement"))
        with self.assertRaisesRegex(acceptance.AcceptanceError, "amendment tests missing"):
            self.check()

    def test_prose_command_is_not_an_amendment_test(self):
        p = self.root / "docs/ADR-0016-engine-assisted-planning.md"
        p.write_text(p.read_text().replace("## Architectural placement",
                     chr(96) + "explain" + chr(96) + " shows the plan.\n## Architectural placement"))
        self.check()

    def test_wrapped_second_amendment_test_is_required(self):
        p = self.root / "docs/ADR-0016-engine-assisted-planning.md"
        p.write_text(p.read_text().replace(chr(96) + "proof" + chr(96),
                     chr(96) + "proof" + chr(96) + " and\n" + chr(96) + "another_proof" + chr(96)))
        with self.assertRaisesRegex(acceptance.AcceptanceError, "amendment tests missing"):
            self.check()

    def test_missing_case_refuses(self):
        self.cases.pop()
        with self.assertRaisesRegex(acceptance.AcceptanceError, "exactly 1 through 24"):
            self.check()

    def test_duplicate_case_refuses(self):
        self.cases[-1][0] = "1"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "duplicate ADR"):
            self.check()

    def test_unregistered_disposition_refuses(self):
        self.cases[0][1] = "good enough"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "invalid disposition"):
            self.check()

    def test_operator_responsibility_names_the_contract(self):
        self.cases[0] = ["1", "operator-vouched", "-", "ADR-0016"]
        with self.assertRaisesRegex(acceptance.AcceptanceError, "SPEC reference"):
            self.check()
        self.cases[0][3] = "SPEC 9.3.2"
        self.check()

    def test_test_disposition_requires_a_mapping(self):
        self.cases[0][2] = "-"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "has no test"):
            self.check()

    def test_unknown_mapping_refuses(self):
        self.cases[0][2] = "invented"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "unknown test references"):
            self.check()

    def test_missing_test_refuses(self):
        self.source.write_text("")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "got 0"):
            self.check()

    def test_comment_or_string_is_not_a_test(self):
        self.source.write_text('// #[test] fn proof() {}\nconst S: &str = r#"#[test] fn proof() {}"#;')
        with self.assertRaisesRegex(acceptance.AcceptanceError, "got 0"):
            self.check()

    def test_helper_function_is_not_a_second_test(self):
        (self.root / "crates/example/src/helper.rs").write_text("fn proof() {}")
        self.check()

    def test_duplicate_test_refuses(self):
        self.source.write_text("#[test] fn proof() {}\n#[test] fn proof() {}")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "got 2"):
            self.check()

    def test_same_leaf_in_another_source_is_not_the_same_reference(self):
        (self.root / "crates/example/src/other.rs").write_text("#[test] fn proof() {}")
        self.check()

    def test_inline_module_qualification_disambiguates_test_leaves(self):
        self.source.write_text(
            'mod first { #[test] #[ignore = "fixture"] fn proof() {} }\n'
            'mod second { #[test] #[ignore = "fixture"] fn proof() {} }\n')
        self.tests[0][1] = "first::proof"
        self.inventory["groups"][0]["cases"] = ["first::proof"]
        self.check()
        self.tests[0][1] = "absent::proof"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "got 0"):
            self.check()

    def test_attributes_do_not_change_the_enclosing_module_depth(self):
        self.source.write_text(
            'mod outer { #[allow(dead_code)] mod inner { #[test] #[ignore] fn proof() {} } }\n'
            '#[test] #[ignore] fn proof() {}\n')
        self.tests[0][1] = "outer::inner::proof"
        self.inventory["groups"][0]["cases"] = ["outer::inner::proof"]
        self.check()
        self.tests[0][1] = "proof"
        self.inventory["groups"][0]["cases"] = ["proof"]
        self.check()

    def test_wrong_source_refuses(self):
        self.tests[0][2] = "crates/example/src/elsewhere.rs"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "source"):
            self.check()

    def test_wrong_job_refuses(self):
        self.tests[0][5] = "live"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "job disagrees"):
            self.check()

    def test_wrong_platform_refuses(self):
        self.tests[0][6] = "win32"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "platform disagrees"):
            self.check()

    def test_wrong_cargo_target_refuses(self):
        self.tests[0][3] = "example/test/elsewhere"
        with self.assertRaisesRegex(acceptance.AcceptanceError, "no matching target"):
            self.check()

    def test_ordinary_test_uses_ordinary_owner(self):
        self.source.write_text("#[test] fn proof() {}")
        with self.assertRaisesRegex(acceptance.AcceptanceError, "ordinary owner"):
            self.check()
        self.tests[0][4:6] = ["quick", "quick"]
        self.check()

    def test_empty_execution_coordinates_refuse(self):
        for field in (7, 8, 9):
            with self.subTest(field=field):
                original = self.tests[0][field]
                self.tests[0][field] = "-"
                with self.assertRaisesRegex(acceptance.AcceptanceError, "missing"):
                    self.check()
                self.tests[0][field] = original


if __name__ == "__main__":
    unittest.main()
