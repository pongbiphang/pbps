#!/usr/bin/env python3
"""Negative controls for discovery and the supported scheduling-witness syntax."""

import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import ignored_test_inventory as audit


KEY = ("demo", "lib", "demo")
WORKFLOW = """jobs:
  live:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test -p demo --lib -- --ignored
"""


class Ownership(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        (self.root / ".github/workflows").mkdir(parents=True)
        self.workflow = self.root / ".github/workflows/ci.yml"
        self.workflow.write_text(WORKFLOW, encoding="utf-8")
        self.inventory = {
            "version": 1,
            "owners": {"live": {"platform": "linux", "fixture": "Owned test database", "job": "live"}},
            "groups": [{"target": list(KEY), "owner": "live", "validate_on": "linux",
                        "ignored_on": ["linux", "win32", "darwin"], "cases": ["owned"]}],
        }
        self.targets = {KEY: {"all": {"owned", "ordinary"}, "ignored": {"owned"}}}

    def check(self, platform="linux"):
        return audit.validate(self.root, self.inventory, self.targets, platform)

    def test_compiled_ignored_case_has_a_real_owner(self):
        self.assertEqual(self.check(), 1)

    def test_ordinary_owners_cannot_hide_ignored_cases_on_their_execution_platform(self):
        self.inventory["owners"]["live"]["ordinary"] = True
        self.workflow.write_text(WORKFLOW.replace(
            "cargo test -p demo --lib -- --ignored", "cargo test --workspace --all-targets"))
        for platform in ("linux", "win32", "darwin"):
            with self.subTest(platform=platform):
                with self.assertRaisesRegex(audit.InventoryError, "ordinary runner skips ignored"):
                    self.check(platform)

    def test_ordinary_owners_may_run_cases_ignored_only_on_another_platform(self):
        self.inventory["owners"]["live"]["ordinary"] = True
        self.inventory["groups"][0]["ignored_on"] = ["darwin"]
        self.targets[KEY]["ignored"].clear()
        self.workflow.write_text(WORKFLOW.replace(
            "cargo test -p demo --lib -- --ignored", "cargo test --workspace --all-targets"))
        for platform in ("linux", "win32"):
            with self.subTest(platform=platform):
                self.assertEqual(self.check(platform), 1)

    def test_new_case_is_refused_until_an_owner_is_registered(self):
        self.targets[KEY]["all"].add("orphan")
        self.targets[KEY]["ignored"].add("orphan")
        with self.assertRaisesRegex(audit.InventoryError, "no execution owner"):
            self.check()

    def test_removed_case_and_wrong_target_are_stale_not_empty_success(self):
        for mutate in (lambda: self.targets[KEY]["all"].clear(),
                       lambda: self.inventory["groups"][0]["target"].__setitem__(2, "missing")):
            with self.subTest(mutation=mutate):
                old_targets, old_inventory = copy.deepcopy(self.targets), copy.deepcopy(self.inventory)
                mutate()
                with self.assertRaisesRegex(audit.InventoryError, "stale case/target"):
                    self.check()
                self.targets, self.inventory = old_targets, old_inventory

    def test_a_removed_runner_or_an_echo_or_comment_is_not_execution(self):
        for line in ("echo done", '# cargo test -p demo --lib -- --ignored',
                     'echo "cargo test -p demo --lib -- --ignored"'):
            with self.subTest(line=line):
                self.workflow.write_text(WORKFLOW.replace("cargo test -p demo --lib -- --ignored", line))
                with self.assertRaisesRegex(audit.InventoryError, "no longer executes"):
                    self.check()

    def test_skip_exact_and_substring_filters_match_libtest(self):
        for arguments in ("--ignored --skip owned", "--ignored --exact own", "--ignored missing"):
            with self.subTest(arguments=arguments):
                self.workflow.write_text(WORKFLOW.replace("--ignored", arguments))
                with self.assertRaisesRegex(audit.InventoryError, "no longer executes"):
                    self.check()
        for arguments in ("--ignored --exact owned", "--ignored own --test-threads=1"):
            self.workflow.write_text(WORKFLOW.replace("--ignored", arguments))
            self.assertEqual(self.check(), 1)

    def test_profile_and_named_multiline_steps_keep_the_same_selection(self):
        self.workflow.write_text(WORKFLOW.replace(
            "      - run: cargo test -p demo --lib -- --ignored",
            "      - name: selected\n        run: |\n          cargo test --profile live-test -p demo --lib -- \\\n            --ignored --exact owned | tee results.log"))
        self.assertEqual(self.check(), 1)

    def test_disabled_or_wrong_platform_jobs_cannot_own_a_case(self):
        for workflow in (WORKFLOW.replace("    steps:", "    if: false\n    steps:"),
                         WORKFLOW.replace("      - run:", "      - name: disabled\n        if: false\n        run:"),
                         WORKFLOW.replace("ubuntu-latest", "windows-latest")):
            with self.subTest(workflow=workflow):
                self.workflow.write_text(workflow)
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_matrix_condition_must_have_an_executed_variant(self):
        workflow = WORKFLOW.replace("    steps:", "    strategy:\n      matrix:\n        engine: [pg, mssql]\n    steps:")
        workflow = workflow.replace("      - run:", "      - name: selected\n        if: matrix.engine == 'pg'\n        run:")
        self.workflow.write_text(workflow)
        self.assertEqual(self.check(), 1)
        self.workflow.write_text(workflow.replace("[pg, mssql]", "[mssql]"))
        with self.assertRaisesRegex(audit.InventoryError, "no longer executes"):
            self.check()

    def test_excluded_or_empty_matrix_is_not_assumed_to_execute(self):
        for matrix in ('engine: []', 'engine: [pg]\n        exclude: [{engine: pg}]'):
            self.workflow.write_text(WORKFLOW.replace(
                '    steps:', '    strategy:\n      matrix:\n        '+matrix+'\n    steps:'))
            with self.assertRaises(audit.InventoryError):
                self.check()

    def test_changed_ignore_condition_requires_inventory_review(self):
        self.targets[KEY]["ignored"].clear()
        with self.assertRaisesRegex(audit.InventoryError, "changed ignore condition"):
            self.check()

    def test_a_platform_only_case_is_checked_on_its_compiling_platform(self):
        self.workflow.write_text(WORKFLOW.replace("ubuntu-latest", "windows-latest"))
        self.inventory["owners"]["live"]["platform"] = "win32"
        self.inventory["groups"][0]["validate_on"] = "win32"
        self.inventory["groups"][0]["ignored_on"] = ["win32"]
        self.assertEqual(self.check("win32"), 1)
        self.targets = {}
        self.assertEqual(self.check("linux"), 1)
        with self.assertRaisesRegex(audit.InventoryError, "stale case/target"):
            self.check("win32")

    def test_a_stale_selector_cannot_hide_in_a_valid_python_runner(self):
        script = self.root / "runner.py"
        script.write_text('TESTS = ["owned", "stale"]\n')
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        with self.assertRaisesRegex(audit.InventoryError, "stale runner selector"):
            self.check()
        script.write_text('TESTS = ["different"]\n')
        with self.assertRaisesRegex(audit.InventoryError, "no longer selects"):
            self.check()

    def test_a_runner_with_mutated_selector_data_has_no_execution_owner(self):
        script = self.root / "runner.py"
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        script.write_text('TESTS = ["owned"]\nALIAS = TESTS\nALIAS.clear()\n')
        with self.assertRaisesRegex(audit.InventoryError, "not supported literal data"):
            self.check()
        script.write_text('TESTS = ["removed"]\nTESTS = ["owned"]\n')
        self.assertEqual(self.check(), 1)

    def test_duplicate_and_cyclic_owners_are_refused(self):
        self.inventory["groups"].append(copy.deepcopy(self.inventory["groups"][0]))
        with self.assertRaisesRegex(audit.InventoryError, "duplicate case"):
            self.check()
        self.inventory["groups"].pop()
        self.inventory["owners"]["live"]["parent"] = "live"
        with self.assertRaisesRegex(audit.InventoryError, "cycle"):
            self.check()

    def test_nested_helpers_require_a_scheduled_compiled_parent_and_live_call(self):
        script = self.root / "parent.rs"
        source = 'fn ordinary() { command.args(["--ignored", "--exact", "owned"]).spawn(); }'
        script.write_text(source)
        self.targets[KEY]["sources"] = {script.resolve()}
        self.inventory["owners"] = {
            "quick": {"platform": "linux", "fixture": "ordinary parent", "job": "live", "ordinary": True},
            "child": {"platform": "linux", "fixture": "parent-owned PID fixture", "parent": "quick",
                      "case": "owned", "parent_tests": [{"target": list(KEY), "case": "ordinary"}],
                      "witnesses": [{"file": "parent.rs", "language": "rust", "scope": "ordinary",
                                     "requires": ['command.args(["--ignored", "--exact", "owned"]).spawn()']}]},
        }
        self.inventory["groups"][0]["owner"] = "child"
        self.workflow.write_text(WORKFLOW.replace("cargo test -p demo --lib -- --ignored", "cargo test --workspace --all-targets"))
        self.assertEqual(self.check(), 1)
        script.write_text('fn ordinary() { /* command.args(["--ignored", "--exact", "owned"]).spawn(); */ }')
        with self.assertRaisesRegex(audit.InventoryError, "removed invocation"):
            self.check()
        script.write_text(source)
        self.targets[KEY]["sources"].clear()
        with self.assertRaisesRegex(audit.InventoryError, "not compiled into its target"):
            self.check()
        self.targets[KEY]["sources"].add(script.resolve())
        self.targets[KEY]["all"].remove("ordinary")
        with self.assertRaisesRegex(audit.InventoryError, "missing parent test"):
            self.check()


class SourceWitnesses(unittest.TestCase):
    def test_python_literals_comments_and_inactive_functions_are_not_calls(self):
        witness = audit.python_tokens('run(binary, "--ignored", "--exact", TEST)')
        for body in ('# run(binary, "--ignored", "--exact", TEST)\n    pass',
                     '"run(binary, ignored, exact, TEST)"',
                     'if False:\n        run(binary, "--ignored", "--exact", TEST)',
                     'def unused():\n        run(binary, "--ignored", "--exact", TEST)',
                     'if True:\n        if False:\n            run(binary, "--ignored", "--exact", TEST)',
                     'callback = lambda: run(binary, "--ignored", "--exact", TEST)'):
            with self.subTest(body=body):
                text = audit.python_scope('def main():\n    '+body+'\n', 'main')
                self.assertFalse(audit.contains(audit.python_tokens(text), witness))
        text = audit.python_scope('def main():\n    if True:\n        run(binary, "--ignored", "--exact", TEST)\n', 'main')
        self.assertTrue(audit.contains(audit.python_tokens(text), witness))

    def test_python_selector_data_supports_literals_prefixes_and_comprehensions(self):
        tree = audit.ast.parse('PREFIX = "module::"\nTESTS = [PREFIX + n for n in ["one", "two"]] + ["three"]')
        self.assertEqual(audit.python_values(tree)["TESTS"], ["module::one", "module::two", "three"])
        for expression in ('__import__("os").system("false")', '[x for x in ["a"] if False]'):
            with self.assertRaises(audit.InventoryError):
                audit.static_value(audit.ast.parse(expression, mode='eval').body, {})

    def test_rust_scope_ignores_nested_comments_raw_strings_and_other_functions(self):
        source = '''/* outer /* fn parent() { fake(); } */ still comment */
const DECOY: &str = r##"fn parent() { fake(); }"##;
fn other() { fake(); }
fn parent<'a>(x: &'a str) { let c = '}'; { real(x); } }
'''
        # Generic function witness scopes are deliberately not accepted: the
        # registered fixture functions have ordinary argument lists.
        with self.assertRaises(audit.InventoryError):
            audit.rust_scope(source, 'parent')
        source = source.replace("parent<'a>(x: &'a str)", 'parent(x: &str)')
        body = audit.rust_scope(source, 'parent')
        self.assertTrue(audit.contains(body, audit.rust_tokens('real(x)')))
        self.assertFalse(audit.contains(body, audit.rust_tokens('fake()')))
        for broken in ('/* unterminated', 'r###"unterminated', '"unterminated'):
            with self.assertRaises(audit.InventoryError):
                audit.rust_tokens(broken)

    def test_ambiguous_rust_module_functions_fail_instead_of_picking_one(self):
        with self.assertRaisesRegex(audit.InventoryError, 'ambiguous'):
            audit.rust_scope('mod a { fn child() {} } mod b { fn child() {} }', 'child')

    def test_shell_comments_echoes_functions_and_guarded_commands_do_not_own_tests(self):
        for script in ('# cargo test -p demo --lib -- --ignored',
                       'echo "cargo test -p demo --lib -- --ignored"',
                       'if false; then\n cargo test -p demo --lib -- --ignored\nfi'):
            self.assertEqual(list(audit.shell_commands(script)), [])
        with self.assertRaises(audit.InventoryError):
            list(audit.shell_commands('unused() {\n cargo test -p demo --lib -- --ignored\n}'))

    def test_malformed_or_duplicate_libtest_output_fails_closed(self):
        self.assertEqual(audit.listed_tests('module::case: test\n'), {'module::case'})
        for output in ('something failed\n', 'case: test\ncase: test\n', 'case: benchmark\n'):
            with self.assertRaises(audit.InventoryError):
                audit.listed_tests(output)

    def test_empty_cargo_artifacts_are_not_an_empty_passing_inventory(self):
        with patch.object(audit.subprocess, 'check_output', side_effect=['{"packages": []}', '']):
            with self.assertRaisesRegex(audit.InventoryError, 'no test artifacts'):
                audit.discover(Path('.'))

    def test_custom_harness_is_refused_before_executing_its_binary(self):
        with tempfile.TemporaryDirectory() as directory:
            manifest = Path(directory) / 'Cargo.toml'
            manifest.write_text('[[test]]\nname = "custom"\nharness = false\n')
            metadata = json.dumps({'packages': [{'id': 'demo', 'name': 'demo', 'manifest_path': str(manifest)}]})
            artifact = json.dumps({'reason': 'compiler-artifact', 'executable': '/must-not-run',
                                   'profile': {'test': True}, 'package_id': 'demo',
                                   'target': {'kind': ['test'], 'name': 'custom'}})
            with patch.object(audit.subprocess, 'check_output', side_effect=[metadata, artifact]) as command:
                with self.assertRaisesRegex(audit.InventoryError, 'custom test harness'):
                    audit.discover(Path(directory))
                self.assertEqual(command.call_count, 2)

    def test_dep_info_preserves_spaces_windows_paths_and_refuses_missing_rules(self):
        self.assertEqual(audit.dep_paths("target/test.d: crates/a\\ b.rs C:\\repo\\module.rs\n"),
                         ["crates/a b.rs", "C:\\repo\\module.rs"])
        for text in ("", "not a rule", "target/test.d: "):
            with self.assertRaises(audit.InventoryError):
                audit.dep_paths(text)

    def test_actual_compilation_owns_cfg_and_path_module_discovery(self):
        with tempfile.TemporaryDirectory(prefix="pbps inventory # ") as directory:
            root = Path(directory)
            (root/'nested.rs').write_text('#[test]\n#[ignore]\nfn nested_case() {}\n')
            (root/'optional.rs').write_text('#[test] #[ignore] fn platform_case() {}')
            (root/'tests.rs').write_text('''#[path = "nested.rs"] mod renamed;
#[cfg(inventory_fixture)] #[path = "optional.rs"] mod conditional;
#[cfg_attr(inventory_fixture, ignore)] #[test] fn conditional_ignore() {}
''')
            listings, sources = [], []
            for enabled in (False, True):
                binary = root / ('with.exe' if enabled else 'without.exe')
                command = ['rustc', '--test', str(root/'tests.rs'), '-o', str(binary), '--emit', 'link,dep-info='+str(root/'test.d')]
                if enabled: command += ['--cfg', 'inventory_fixture']
                subprocess.run(command, check=True, capture_output=True, text=True)
                sources.append(set(audit.dep_paths((root/'test.d').read_text())))
                listings.append(audit.listed_tests(subprocess.check_output(
                    [str(binary), '--list', '--ignored', '--format', 'terse'], text=True)))
            self.assertNotIn(str(root/'optional.rs'), sources[0])
            self.assertIn(str(root/'optional.rs'), sources[1])
            self.assertEqual(listings[0], {'renamed::nested_case'})
            self.assertEqual(listings[1], {'renamed::nested_case', 'conditional::platform_case', 'conditional_ignore'})

    def test_unsupported_selector_writes_cannot_retain_a_previous_literal(self):
        writes = (
            'TESTS = dynamic_cases()', 'TESTS += dynamic_cases()', 'del TESTS',
            'TESTS: list = dynamic_cases()', 'TESTS, other = dynamic_cases()',
            '[other, TESTS] = dynamic_cases()', 'TESTS = other = dynamic_cases()',
            'if condition:\n    TESTS = dynamic_cases()',
            'for TESTS in dynamic_cases():\n    pass',
            'with context() as TESTS:\n    pass', 'other = (TESTS := dynamic_cases())',
            'import unknown as TESTS', 'from unknown import TESTS', 'from unknown import *',
            'def TESTS():\n    pass', 'class TESTS:\n    pass',
            'try:\n    run()\nexcept Exception as TESTS:\n    pass',
            'match value:\n    case {"key": TESTS}:\n        pass',
        )
        for write in writes:
            with self.subTest(write=write):
                values = audit.python_values(audit.ast.parse('TESTS = ["owned"]\n' + write))
                self.assertNotIn("TESTS", values)

    def test_visible_mutation_invalidates_every_shared_selector_alias(self):
        mutations = (
            'ALIAS.clear()', 'ALIAS[0] = "changed"', 'del ALIAS[:]',
            'ALIAS += ["changed"]', 'mutate(ALIAS)', 'mutate(tests=ALIAS)',
            'BOX = [ALIAS]\nBOX[0].clear()',
            'BOX = {"tests": ALIAS}\nBOX["tests"].clear()',
            'def helper(tests=ALIAS):\n    tests.clear()',
            'def helper() -> ALIAS:\n    pass',
            'annotation: ALIAS',
            'class Holder:\n    cases = ALIAS\nHolder.cases.clear()',
            'class Outer:\n    TESTS = []\n    class Inner:\n        cases = ALIAS',
        )
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                values = audit.python_values(audit.ast.parse(
                    'TESTS = ["owned"]\nALIAS = TESTS\n' + mutation))
                self.assertNotIn("TESTS", values)

    def test_literal_replacement_does_not_mutate_old_aliases_or_copies(self):
        values = audit.python_values(audit.ast.parse(
            'TESTS = ["owned"]\nALIAS = TESTS\nCOPY = TESTS + []\n'
            'TESTS = ["fresh"]\nALIAS.clear()'))
        self.assertEqual(values["TESTS"], ["fresh"])
        self.assertEqual(values["COPY"], ["owned"])
        self.assertNotIn("ALIAS", values)
        values = audit.python_values(audit.ast.parse(
            'PREFIX = "module::"\nALIAS = PREFIX\nPREFIX = dynamic_prefix()\n'
            'TESTS = [ALIAS + n for n in ["one", "two"]]'))
        self.assertEqual(values["TESTS"], ["module::one", "module::two"])

    def test_local_bindings_and_annotations_do_not_replace_module_selectors(self):
        source = ('TESTS = ["owned"]\nTESTS: list\n'
                  'def helper(TESTS=None):\n    TESTS = []\n'
                  'class Holder:\n    TESTS = []\n    TESTS.clear()\n'
                  'OTHER = [TESTS for TESTS in ["local"]]\n')
        self.assertEqual(audit.python_values(audit.ast.parse(source))["TESTS"], ["owned"])
        values = audit.python_values(audit.ast.parse(
            'TESTS = ["owned"]\nclass Holder:\n    global TESTS\n    TESTS = []'))
        self.assertNotIn("TESTS", values)

    def test_executed_class_global_writes_invalidate_module_selector_data(self):
        for body in (
            'class Holder:\n    if True:\n        global TESTS\n        TESTS = []',
            'class Outer:\n    class Inner:\n        global TESTS\n        TESTS = []',
        ):
            with self.subTest(body=body):
                source = 'TESTS = ["owned"]\n' + body
                actual = subprocess.run([sys.executable, '-c', source + '\nprint(TESTS)'],
                                        capture_output=True, text=True, check=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                self.assertNotIn("TESTS", audit.python_values(audit.ast.parse(source)))

    def test_stale_selector_refusal_agrees_with_real_python_execution(self):
        variants = (
            'TESTS = dynamic_cases()', 'del TESTS', 'TESTS: list = []',
            'TESTS, other = [], None', 'TESTS = other = []',
            'ALIAS = TESTS\nALIAS.clear()', 'TESTS[:] = []',
            'if bool("yes"):\n    TESTS = []',
            'BOX = {"tests": TESTS}\nBOX["tests"].clear()',
            'other = (TESTS := [])',
            'def helper() -> TESTS:\n    pass\nhelper.__annotations__["return"].clear()',
            'annotation: TESTS\n__annotations__["annotation"].clear()',
            'globals()["TESTS"] = []', 'exec("TESTS = []")',
            'namespace = locals()\nnamespace["TESTS"] = []',
        )
        for variant in variants:
            with self.subTest(variant=variant):
                source = 'def dynamic_cases():\n    return []\nTESTS = ["owned"]\n' + variant
                actual = subprocess.run(
                    [sys.executable, '-c', source + '\nimport json\nprint(json.dumps(globals().get("TESTS", [])))'],
                    capture_output=True, text=True, check=True, timeout=10)
                self.assertEqual(json.loads(actual.stdout), [])
                with self.assertRaises(audit.InventoryError):
                    audit.static_value(audit.ast.parse('TESTS', mode='eval').body,
                                       audit.python_values(audit.ast.parse(source)))

if __name__ == '__main__':
    unittest.main()
