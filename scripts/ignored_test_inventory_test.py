#!/usr/bin/env python3
"""Negative controls for discovery and the supported scheduling-witness syntax."""

import copy
import json
import os
import re
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

    def test_cargo_filter_cannot_claim_an_unexecuted_case(self):
        for name in ("missing", "own"):
            self.workflow.write_text(WORKFLOW.replace("--lib --", f"--lib {name} --")
                                     .replace("--ignored", "--ignored --exact"))
            with self.subTest(name=name):
                with self.assertRaisesRegex(audit.InventoryError, "no longer executes"):
                    self.check()
        self.workflow.write_text(WORKFLOW.replace("--lib --", "--lib owned --"))
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


class CargoSelectors(unittest.TestCase):
    def test_ambiguous_or_unsupported_cargo_arguments_supply_no_owner(self):
        for arguments in (["--profile"], ["--profile", "--lib"], ["--profile="],
                          ["--package"], ["--test"], ["--bin"], ["--offline", "--offline"],
                          ["--unknown", "owned"], ["--help"],
                          ["owned", "other"], ["--test", "integration"],
                          ["--package", "other"], ["--lib"], ["--profile", "a", "--profile", "b"]):
            with self.subTest(arguments=arguments):
                with self.assertRaises(audit.InventoryError):
                    audit.cargo_selection(["cargo", "test", "-p", "demo", "--lib",
                                           *arguments, "--", "--ignored"])
        self.assertIsNone(audit.cargo_selection(
            ["cargo", "test", "-p", "demo", "--lib", "owned", "--no-run", "--", "--ignored"]))

    def test_supported_cargo_filters_agree_with_actual_executed_cases(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "src").mkdir()
            (root / "Cargo.toml").write_text(
                '[package]\nname="demo"\nversion="0.0.0"\nedition="2021"\n'
                '[profile.live-test]\ninherits="test"\n', encoding="utf-8")
            (root / "src/lib.rs").write_text(
                '#[test] #[ignore] fn owned() {}\n#[test] #[ignore] fn other() {}\n', encoding="utf-8")
            environment = dict(os.environ, CARGO_TARGET_DIR=str(root / "target"))
            cases = [
                (["missing"], ["--ignored"], set()),
                (["owned"], ["--ignored"], {"owned"}),
                (["own"], ["--ignored"], {"owned"}),
                (["own"], ["--ignored", "--exact"], set()),
                (["owned"], ["--ignored", "--exact"], {"owned"}),
                (["missing"], ["--ignored", "owned"], {"owned"}),
                (["owned"], ["--ignored", "other"], {"owned", "other"}),
                (["owned"], ["--ignored", "--exact", "other"], {"owned", "other"}),
                (["owned"], ["--ignored", "--skip", "owned"], set()),
                (["owned"], ["--ignored", "--exact", "--skip", "owned"], set()),
                (["--profile", "live-test"], ["--ignored"], {"owned", "other"}),
                (["--profile", "live-test", "owned"], ["--ignored"], {"owned"}),
                (["--profile=live-test", "owned"], ["--ignored"], {"owned"}),
            ]
            for before, after, expected in cases:
                with self.subTest(before=before, after=after):
                    command = ["cargo", "test", "--offline", "-p", "demo", "--lib",
                               *before, "--", *after]
                    result = subprocess.run(command, cwd=root, env=environment, check=True,
                                            capture_output=True, text=True, timeout=120)
                    actual = set(re.findall(r"^test (\w+) \.\.\. ok$", result.stdout, re.M))
                    self.assertEqual(actual, expected, result.stdout + result.stderr)
                    selection = audit.cargo_selection(command)
                    self.assertEqual({name for name in ("owned", "other")
                                      if audit.selects(selection, KEY, name)}, actual)


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

    def test_unreachable_script_guards_cannot_supply_an_execution_witness(self):
        witness = audit.python_tokens("main()")
        for source in ('if __name__ == "__never__": main()',
                       'if __name__ != "__main__": main()',
                       'if "__main__" != __name__: main()',
                       'if not __name__: main()',
                       'if True:\n    if __name__ == "imported": main()',
                       'if __name__ == "__main__": pass\nelse: main()'):
            with self.subTest(source=source):
                tokens = audit.python_tokens(audit.python_scope(source, "<module>"))
                self.assertFalse(audit.contains(tokens, witness))

    def test_real_script_entry_and_selected_else_branches_remain_witnesses(self):
        witness = audit.python_tokens("main()")
        for source in ('main()', 'if __name__ == "__main__": main()',
                       'if "__main__" == __name__: main()',
                       'if __name__ != "imported": raise SystemExit(main())',
                       'if __name__ == "imported": pass\nelse: main()',
                       'if False: pass\nelif __name__ == "__main__": main()'):
            with self.subTest(source=source):
                tokens = audit.python_tokens(audit.python_scope(source, "<module>"))
                self.assertTrue(audit.contains(tokens, witness))

    def test_unknown_module_conditions_and_control_forms_supply_no_witness(self):
        witness = audit.python_tokens("main()")
        for source in ('if enabled: main()', 'if discover(): main()',
                       'if enabled: pass\nelse: main()',
                       'while __name__ == "imported": main()',
                       'for item in unknown: main()',
                       'try: pass\nexcept Exception: main()',
                       'with context(): main()',
                       'match __name__:\n    case "imported": main()'):
            with self.subTest(source=source):
                tokens = audit.python_tokens(audit.python_scope(source, "<module>"))
                self.assertFalse(audit.contains(tokens, witness))

    def test_module_guards_do_not_remove_function_runtime_branches_or_direct_calls(self):
        witness = audit.python_tokens("main()")
        source = 'if os.geteuid() != 0: parser.error("needs root")\nmain()'
        self.assertTrue(audit.contains(audit.python_tokens(audit.python_scope(source, "<module>")), witness))
        source = 'def fixture(args):\n    if args.native_host: main()'
        self.assertTrue(audit.contains(audit.python_tokens(audit.python_scope(source, "fixture")), witness))

    def test_a_rebound_module_name_is_not_assumed_to_be_the_script_entry(self):
        for binding in ('__name__ = "imported"', 'del __name__',
                        'import sys as __name__', 'from sys import version as __name__',
                        'def __name__(): pass', 'class __name__: pass',
                        'if enabled: __name__ = "imported"',
                        'try: pass\nexcept Exception as __name__: pass',
                        'match "imported":\n    case __name__: pass',
                        'def helper(value=(__name__ := "imported")): pass'):
            with self.subTest(binding=binding):
                with self.assertRaisesRegex(audit.InventoryError, "module entry name is rebound"):
                    audit.python_scope(binding + '\nif __name__ == "__main__": main()', "<module>")
        for definition in ('def unused():\n    __name__ = "local"',
                           'unused = lambda: (__name__ := "local")'):
            source = definition + '\nif __name__ == "__main__": main()'
            self.assertTrue(audit.contains(audit.python_tokens(audit.python_scope(source, "<module>")),
                                           audit.python_tokens("main()")))

    def test_module_witness_agrees_with_a_real_script_entry(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            script = root / "runner.py"
            witness = {"file": "runner.py", "language": "python", "scope": "<module>",
                       "requires": ["main()"]}
            for guard, runs in [('__name__ == "__main__"', True),
                                ('__name__ == "__never__"', False),
                                ('__name__ != "__main__"', False),
                                ('(1,) == [1]', False), ('[1] == (1,)', False)]:
                with self.subTest(guard=guard):
                    script.write_text('def main(): print("selected")\nif ' + guard + ': main()\n',
                                      encoding="utf-8")
                    actual = subprocess.run([sys.executable, str(script)], check=True,
                                            capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, "selected\n" if runs else "")
                    if runs:
                        audit.check_witness(root, witness)
                    else:
                        with self.assertRaisesRegex(audit.InventoryError, "removed invocation"):
                            audit.check_witness(root, witness)

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


if __name__ == '__main__':
    unittest.main()
