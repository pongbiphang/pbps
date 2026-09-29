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
from textwrap import indent
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

    def test_immutable_tuple_selectors_survive_read_calls_and_aliases(self):
        controls = [
            ('TESTS = ("owned",)', "('owned',)"),
            ('TESTS = ("owned",)\nprint(TESTS)', "('owned',)"),
            ('TESTS = ("owned",)\ndef consume(value): return len(value)\nconsume(TESTS)', "('owned',)"),
            ('TESTS = ("owned",)\nALIAS = TESTS\nprint(ALIAS)', "('owned',)"),
            ('TESTS = ("owned",)\nBOX = ((TESTS,),)\nprint(BOX)', "('owned',)"),
            ('TESTS = ("owned",) + ()\nprint(TESTS)', "('owned',)"),
            ('TESTS = ("owned",)\nBOX = ([], TESTS)\nBOX[0].clear()', "('owned',)"),
            ('TESTS = [name for name in ("owned",)]', "['owned']"),
            ('TESTS = ["owned"]\nCOPY = TESTS + []\nBOX = (COPY,)\nBOX[0].clear()', "['owned']"),
            ('PREFIX = "ow"\nTESTS = (PREFIX + "ned",)\nprint(TESTS)', "('owned',)"),
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for source, expected in controls:
            with self.subTest(source=source):
                actual = subprocess.run([sys.executable, "-c", source + '\nprint(repr(TESTS))'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout.splitlines()[-1], expected)
                (self.root / "runner.py").write_text(source + '\n', encoding="utf-8")
                self.assertEqual(self.check(), 1)

    def test_tuple_wrappers_do_not_hide_mutable_selector_children(self):
        mutations = [
            'BOX = (TESTS,)\nBOX[0].clear()',
            'BOX = ((TESTS,),)\nBOX[0][0].clear()',
            'BOX = [(TESTS,)]\nBOX[0][0].clear()',
            'BOX = (TESTS,)\ndef mutate(value): value[0].clear()\nmutate(BOX)',
            'BOX = (TESTS,) + ()\nBOX[0].clear()',
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                source = 'TESTS = ["owned"]\n' + mutation + '\n'
                # Observe separately: print in the audited source would itself
                # invalidate the list and hide missing tuple-child tracking.
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "[]\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_mixed_tuple_list_concatenation_cannot_invent_a_selector(self):
        for literal, suffix in [('("owned",)', '[]'), ('["owned"]', '()')]:
            for expression_only in (False, True):
                with self.subTest(literal=literal, expression_only=expression_only):
                    source = f'PARTS = {literal}\n'
                    assignment = f'TESTS = PARTS + {suffix}\n'
                    actual = subprocess.run([sys.executable, "-c", source + assignment],
                                            capture_output=True, text=True, timeout=10)
                    self.assertNotEqual(actual.returncode, 0)
                    self.assertIn("TypeError", actual.stderr)
                    self.inventory["owners"]["live"]["selection"] = {
                        "kind": "data", "file": "runner.py",
                        "expression": f'PARTS + {suffix}' if expression_only else "TESTS"}
                    (self.root / "runner.py").write_text(
                        source if expression_only else source + assignment, encoding="utf-8")
                    with self.assertRaises(audit.InventoryError):
                        self.check()

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

    def test_unsupported_assignments_cannot_escape_mutable_selector_aliases(self):
        mutations = [
            'BOX = []\nBOX += [TESTS]\nBOX[0].clear()',
            'BOX = list()\nBOX += [TESTS]\nBOX[0].clear()',
            'if (ALIAS := TESTS): pass\nALIAS.clear()',
            'for _ in [0]: ALIAS = TESTS\nALIAS.clear()',
            'try: ALIAS = TESTS\nfinally: pass\nALIAS.clear()',
            'if True: ALIAS: list = TESTS\nALIAS.clear()',
            'if True: BOX = {"nested": TESTS}\nBOX["nested"].clear()',
            'if True: ALIAS = SECOND = TESTS\nSECOND.clear()',
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for mutation in mutations:
            with self.subTest(mutation=mutation):
                source = 'TESTS = ["owned"]\n' + mutation + '\n'
                # Observe real execution separately: adding print(TESTS) to
                # the analyzed fixture would itself look like an escape.
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "[]\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_supported_copies_and_immutable_rhs_preserve_selectors(self):
        controls = [
            'ALIAS = TESTS',
            'COPY = TESTS + []\nBOX = []\nBOX += [COPY]\nBOX[0].clear()',
            'COPY = [name for name in TESTS]\nif True: ALIAS = COPY\nALIAS.clear()',
            'LABEL = "owned"\nBOX = []\nBOX += [LABEL]\nBOX.clear()\nTESTS = [LABEL]',
            'LABEL = "owned"\nif (ALIAS := LABEL): pass\nTESTS = [LABEL]',
            'def unused():\n    ALIAS = TESTS\n    ALIAS.clear()',
            'class Local:\n    TESTS = []\n    ALIAS = TESTS\n    ALIAS.clear()',
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for control in controls:
            with self.subTest(control=control):
                source = 'TESTS = ["owned"]\n' + control + '\n'
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                self.assertEqual(self.check(), 1)

    def test_deleted_class_shadows_cannot_hide_module_selector_escapes(self):
        escape = 'ALIAS = TESTS\nALIAS.clear()'
        bodies = [
            'del TESTS\n' + escape,
            'if True:\n    del TESTS\n' + escape,
            'for _ in [0]:\n    del TESTS\n' + escape,
            'try:\n    del TESTS\nfinally:\n    pass\n' + escape,
            'try:\n    pass\nfinally:\n    del TESTS\n' + escape,
            'from contextlib import nullcontext\nwith nullcontext():\n    del TESTS\n' + escape,
            'if True:\n    del TESTS\n' + indent(escape, '    '),
            'try:\n    raise ValueError("fixture")\nexcept ValueError as TESTS:\n    pass\n' + escape,
            'try:\n    raise ValueError("fixture")\nexcept ValueError as TESTS:\n    pass\nfinally:\n'
            + indent(escape, '    '),
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for body in bodies:
            with self.subTest(body=body):
                local = 'TESTS = ["local"]\n' + body
                source = 'TESTS = ["owned"]\nclass Local:\n' + indent(local, '    ') + '\n'
                # Keep observation out of the audited source: print itself
                # would otherwise supply a mutable escape and mask the bug.
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "[]\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_other_scope_and_item_deletions_preserve_class_local_shadows(self):
        bodies = [
            '',
            'def unused():\n    del TESTS',
            'def unused():\n    try: raise ValueError("fixture")\n    except ValueError as TESTS: pass',
            'class Nested:\n    TESTS = []\n    if True: del TESTS',
            'class Nested:\n    TESTS = []\n    try: raise ValueError("fixture")\n    except ValueError as TESTS: pass',
            'del TESTS[0]',
            'del TESTS[:]',
            'class Nested:\n    TESTS = []\ndel Nested.TESTS',
            'del TESTS\nTESTS = []',
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for body in bodies:
            with self.subTest(body=body):
                local = 'TESTS = ["local"]\n' + body + '\nALIAS = TESTS\nALIAS.clear()'
                source = 'TESTS = ["owned"]\nclass Local:\n' + indent(local, '    ') + '\n'
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                self.assertEqual(self.check(), 1)

    def test_escaped_namespaces_can_change_selectors_assigned_later(self):
        captures = [
            'namespace = globals()',
            'namespace = locals()',
            'namespace = vars()',
            'original = globals()\nnamespace = original',
            'import sys\nnamespace = vars(sys.modules[__name__])',
            'import sys\nmodule = sys.modules[__name__]\nnamespace = vars(module)',
            'class Holder:\n    namespace = globals()\nnamespace = Holder.namespace',
            'def holder(namespace=globals()): pass\nnamespace = holder.__defaults__[0]',
            'holder = lambda namespace=globals(): None\nnamespace = holder.__defaults__[0]',
            'namespace = eval("globals()")',
            'exec("namespace = globals()")',
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for capture in captures:
            for previous in ('', 'TESTS = ["previous"]\n'):
                with self.subTest(capture=capture, previous=previous):
                    source = previous + capture + '\nTESTS = ["owned"]\nnamespace["TESTS"] = []\n'
                    # Observing TESTS in the audited source would itself look
                    # like an escape and could hide the namespace regression.
                    actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, "[]\n")
                    (self.root / "runner.py").write_text(source, encoding="utf-8")
                    with self.assertRaises(audit.InventoryError):
                        self.check()

    def test_fresh_object_namespaces_do_not_expose_module_selectors(self):
        imports = [
            ('from types import SimpleNamespace', 'SimpleNamespace'),
            ('from types import SimpleNamespace as Namespace', 'Namespace'),
            ('import types', 'types.SimpleNamespace'),
            ('import types as kinds', 'kinds.SimpleNamespace'),
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for import_, constructor in imports:
            inspect = f'namespace = vars({constructor}())\nnamespace["TESTS"] = []\n'
            for statements in (inspect + 'TESTS = ["owned"]\n',
                               'TESTS = ["owned"]\n' + inspect,
                               'TESTS = ["owned"]\n' + inspect * 2 + f'vars({constructor}())\n'):
                with self.subTest(import_=import_, statements=statements):
                    source = import_ + '\n' + statements
                    actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, "['owned']\n")
                    (self.root / "runner.py").write_text(source, encoding="utf-8")
                    self.assertEqual(self.check(), 1)
        source = ('TESTS = ["previous"]\ndef unused(): return globals()\n'
                  'unused_lambda = lambda: globals()\nTESTS = ["owned"]\n')
        actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                check=True, capture_output=True, text=True, timeout=10)
        self.assertEqual(actual.stdout, "['owned']\n")
        (self.root / "runner.py").write_text(source, encoding="utf-8")
        self.assertEqual(self.check(), 1)

    def test_shadowed_namespace_helpers_cannot_claim_safe_object_inspection(self):
        controls = [
            ('from types import SimpleNamespace\n'
             'def SimpleNamespace(): return sys.modules[__name__]', 'SimpleNamespace'),
            ('from types import SimpleNamespace as Namespace\n'
             'Namespace = lambda: sys.modules[__name__]', 'Namespace'),
            ('import types\ntypes.SimpleNamespace = lambda: sys.modules[__name__]',
             'types.SimpleNamespace'),
            ('import types as kinds\n'
             'class Replacement:\n    SimpleNamespace = staticmethod(lambda: sys.modules[__name__])\n'
             'kinds = Replacement', 'kinds.SimpleNamespace'),
            ('from types import SimpleNamespace\ndef vars(ignored): return globals()', 'SimpleNamespace'),
            ('from types import SimpleNamespace\nimport builtins\n'
             'builtins.vars = lambda ignored: globals()', 'SimpleNamespace'),
            ('from types import SimpleNamespace\nimport builtins as defaults\n'
             'defaults.vars = lambda ignored: globals()', 'SimpleNamespace'),
            ('from types import SimpleNamespace\nimport builtins\n'
             'def replace(module): module.vars = lambda ignored: globals()\n'
             'replace(builtins)', 'SimpleNamespace'),
            ('import types\n'
             'def replace(module): module.SimpleNamespace = lambda: sys.modules[__name__]\n'
             'replace(types)', 'types.SimpleNamespace'),
            ('import types\nalias = types\nalias.SimpleNamespace = lambda: sys.modules[__name__]',
             'types.SimpleNamespace'),
            ('import types as first\nimport types as second\n'
             'first.SimpleNamespace = lambda: sys.modules[__name__]', 'second.SimpleNamespace'),
            ('import types\ntypes.SimpleNamespace = lambda: sys.modules[__name__]\nimport types',
             'types.SimpleNamespace'),
            ('import types\ntypes.SimpleNamespace = lambda: sys.modules[__name__]\n'
             'from types import SimpleNamespace', 'SimpleNamespace'),
        ]
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for setup, constructor in controls:
            with self.subTest(setup=setup):
                source = ('import sys\n' + setup + f'\nnamespace = vars({constructor}())\n'
                          'TESTS = ["owned"]\nnamespace["TESTS"] = []\n')
                actual = subprocess.run([sys.executable, "-c", source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "[]\n")
                (self.root / "runner.py").write_text(source, encoding="utf-8")
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_supported_literal_expressions_preserve_fresh_namespace_provenance(self):
        statements = [
            'PREFIX = "own"\nTESTS = [PREFIX + "ed"]',
            'EMPTY = []\nNAMES = ["owned"]\nTESTS = EMPTY + NAMES',
            'EMPTY = ()\nTESTS = EMPTY + ("owned",)',
            'PREFIX = "own"\nPARTS = ["ed"]\nTESTS = [PREFIX + part for part in PARTS]',
            'TESTS = [name for name in ("owned",)]',
            'PREFIX = "ow" + "n"\nALIAS = PREFIX\nTESTS = [ALIAS + "ed"]',
            'TESTS = ["own" + "ed"]\nALIAS = TESTS\nTESTS = ALIAS + []',
            'PREFIX = "own"\nTESTS = ["owned"]\nPREFIX + "ed"',
            'PREFIX = "own"\ndef helper(value=PREFIX + "ed"): pass\nTESTS = ["owned"]',
            'PREFIX = "own"\nhelper = lambda value=PREFIX + "ed": None\nTESTS = ["owned"]',
            'PREFIX = "own"\nasync def helper(value=[PREFIX + name for name in ["ed"]]): pass\nTESTS = ["owned"]',
            'PREFIX = "own"\ndef helper() -> PREFIX + "ed": pass\nTESTS = ["owned"]',
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for import_, constructor in (('from types import SimpleNamespace', 'SimpleNamespace'),
                                     ('import types as kinds', 'kinds.SimpleNamespace')):
            for statement in statements:
                with self.subTest(import_=import_, statement=statement):
                    source = (import_ + '\n' + statement + '\n'
                              f'namespace = vars({constructor}())\nnamespace["TESTS"] = []\n')
                    actual = subprocess.run([sys.executable, '-c', source + 'print(list(TESTS))'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, "['owned']\n")
                    (self.root / 'runner.py').write_text(source, encoding='utf-8')
                    self.assertEqual(self.check(), 1)

    def test_literal_proofs_do_not_restore_provenance_after_opaque_execution(self):
        patch_type = 'types.SimpleNamespace = lambda: sys.modules["__main__"]'
        (self.root / 'side_effect.py').write_text('import sys, types\n' + patch_type + '\n',
                                                encoding='utf-8')
        setups = [
            f'def replace():\n    {patch_type}\nreplace()',
            'import side_effect',
            f'def names():\n    {patch_type}\n    return []\n[name for name in names()]',
            f'class Operand:\n    def __add__(self, other):\n        {patch_type}\n        return \"\"\n'
            'value = Operand()\nvalue + 1',
            f'class Names:\n    def __iter__(self):\n        {patch_type}\n        return iter([])\n'
            '[name for name in Names()]',
        ]
        selectors = ['TESTS = ["own" + "ed"]',
                     'NAMES = ("owned",)\nTESTS = NAMES + ()',
                     'PREFIX = "own"\nTESTS = [PREFIX + name for name in ["ed"]]']
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for setup in setups:
            for selector in selectors:
                with self.subTest(setup=setup, selector=selector):
                    source = ('import sys, types\n' + setup + '\n' + selector + '\n'
                              'namespace = vars(types.SimpleNamespace())\nnamespace["TESTS"] = []\n')
                    actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                            cwd=self.root, check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, '[]\n')
                    (self.root / 'runner.py').write_text(source, encoding='utf-8')
                    with self.assertRaises(audit.InventoryError):
                        self.check()

    def test_unproven_expressions_cannot_supply_literal_provenance(self):
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for expression in ('sys.version + ""', '"owned".lower()', '[name for name in iter(["owned"])]'):
            with self.subTest(expression=expression):
                source = ('import sys\nfrom types import SimpleNamespace\n' + expression + '\n'
                          'TESTS = ["owned"]\nvars(SimpleNamespace())\n')
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                # Actual purity alone is insufficient: fixture code is not
                # executed to extend the auditor's bounded literal grammar.
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_proven_literal_selectors_still_lose_escaped_mutable_aliases(self):
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for argument in ('ALIAS', '(ALIAS,)'):
            with self.subTest(argument=argument):
                source = ('from types import SimpleNamespace\n'
                          'def clear(value):\n'
                          '    if isinstance(value, tuple): value[0].clear()\n'
                          '    else: value.clear()\n'
                          'TESTS = ["own" + "ed"]\nALIAS = TESTS\n'
                          'vars(SimpleNamespace())\n' + f'clear({argument})\n')
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_opaque_execution_cannot_preserve_fresh_namespace_provenance(self):
        patch_type = 'types.SimpleNamespace = lambda: sys.modules[__name__]'
        setups = {
            'implicit module': 'setattr(__builtins__, "vars", lambda ignored: globals())',
            'implicit dictionary': '__builtins__.__dict__["vars"] = lambda ignored: globals()',
            'dictionary builtins': ('__builtins__ = __builtins__.__dict__\n'
                                    '__builtins__["vars"] = lambda ignored: globals()'),
            'dynamic import': '__import__("types").SimpleNamespace = lambda: sys.modules[__name__]',
            'dynamic import alias': 'load = __import__\nload("types").SimpleNamespace = lambda: sys.modules[__name__]',
            'zero argument call': f'def replace():\n    {patch_type}\nreplace()',
            'callable alias': f'def replace():\n    {patch_type}\ncallback = replace\ncallback()',
            'function decorator': (f'def replace(value):\n    {patch_type}\n    return value\n'
                                   '@replace\ndef decorated(): pass'),
            'class decorator': (f'def replace(value):\n    {patch_type}\n    return value\n'
                                '@replace\nclass Decorated: pass'),
            'subclass hook': ('class Base:\n    def __init_subclass__(cls):\n'
                              f'        {patch_type}\nclass Child(Base): pass'),
            'metaclass hook': ('class Meta(type):\n    def __new__(meta, name, bases, body):\n'
                               f'        {patch_type}\n'
                               '        return super().__new__(meta, name, bases, body)\n'
                               'class Child(metaclass=Meta): pass'),
        }
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for label, setup in setups.items():
            for import_ in ('import types', 'from types import SimpleNamespace'):
                constructor = 'types.SimpleNamespace' if import_ == 'import types' else 'SimpleNamespace'
                with self.subTest(label=label, import_=import_):
                    source = ('import sys\nimport types\n' + setup + '\n' + import_ + '\n'
                              f'namespace = vars({constructor}())\nTESTS = ["owned"]\n'
                              'namespace["TESTS"] = []\n')
                    actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, '[]\n')
                    (self.root / 'runner.py').write_text(source, encoding='utf-8')
                    with self.assertRaises(audit.InventoryError):
                        self.check()

    def test_imported_module_execution_cannot_restore_namespace_provenance(self):
        (self.root / 'side_effect.py').write_text(
            'import sys, types\ntypes.SimpleNamespace = lambda: sys.modules["__main__"]\n',
            encoding='utf-8')
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for imports in ('import types\nimport side_effect', 'import side_effect\nimport types',
                        'import side_effect, types', 'import types, side_effect'):
            with self.subTest(imports=imports):
                source = (imports + '\nnamespace = vars(types.SimpleNamespace())\n'
                          'TESTS = ["owned"]\nnamespace["TESTS"] = []\n')
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        cwd=self.root, check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_a_fresh_rhs_cannot_prove_assignment_targets_are_inert(self):
        controls = [
            ('', 'sys.modules[__name__].TESTS'),
            ('', 'sys.modules[__name__].__dict__["TESTS"]'),
            ('', 'namespace = sys.modules[__name__].TESTS'),
            ('class Holder:\n    def __setattr__(self, name, value): globals()["TESTS"] = []\n'
             'holder = Holder()\n', 'holder.value'),
            ('class Holder:\n    def __setitem__(self, key, value): globals()["TESTS"] = []\n'
             'holder = Holder()\n', 'holder[0]'),
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for setup, target in controls:
            with self.subTest(target=target):
                # The first three have no earlier opaque call or class whose
                # refusal could mask the current assignment-target boundary.
                source = ('import sys\n' + setup + 'from types import SimpleNamespace\n'
                          'TESTS = ["owned"]\n' + target + ' = vars(SimpleNamespace())\n')
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS == ["owned"])'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, 'False\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_final_import_binding_determines_the_fresh_constructor(self):
        cases = [
            ('import types as kinds, sys as kinds', 'kinds.SimpleNamespace', False),
            ('import types as kinds, builtins as kinds', 'kinds.SimpleNamespace', False),
            ('from types import SimpleNamespace as N, ModuleType as N', 'N', False),
            ('import sys as kinds, types as kinds', 'kinds.SimpleNamespace', True),
            ('import builtins as kinds, types as kinds', 'kinds.SimpleNamespace', True),
            ('from types import ModuleType as N, SimpleNamespace as N', 'N', True),
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for imports, constructor, valid in cases:
            with self.subTest(imports=imports):
                # No monkey-patch setup: an earlier opaque-execution guard
                # must not mask this import-binding boundary. Invalid final
                # bindings fail in Python before they can select any tests.
                source = imports + f'\nnamespace = vars({constructor}())\nTESTS = ["owned"]\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        capture_output=True, text=True, timeout=10)
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                if valid:
                    self.assertEqual(actual.returncode, 0, actual.stderr)
                    self.assertEqual(actual.stdout, "['owned']\n")
                    self.assertEqual(self.check(), 1)
                else:
                    self.assertNotEqual(actual.returncode, 0)
                    self.assertEqual(actual.stdout, '')
                    self.assertRegex(actual.stderr, 'AttributeError|TypeError')
                    with self.assertRaises(audit.InventoryError):
                        self.check()

        # Keep the original mutation repro too; the earlier opaque target
        # correctly invalidates provenance, while the controls above isolate
        # import ordering without that separate guard masking the boundary.
        source = ('import sys\nsys.SimpleNamespace = lambda: sys.modules[__name__]\n'
                  'import types as kinds, sys as kinds\n'
                  'namespace = vars(kinds.SimpleNamespace())\n'
                  'TESTS = ["owned"]\nnamespace["TESTS"] = []\n')
        actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                check=True, capture_output=True, text=True, timeout=10)
        self.assertEqual(actual.stdout, '[]\n')
        (self.root / 'runner.py').write_text(source, encoding='utf-8')
        with self.assertRaises(audit.InventoryError):
            self.check()

    def test_inert_helpers_and_fresh_dictionaries_preserve_the_exemption(self):
        setups = [
            'def unused():\n    types.SimpleNamespace = lambda: globals()\n',
            'async def unused():\n    types.SimpleNamespace = lambda: globals()\n',
            'unused = lambda: globals()\n',
            'def unused(): return globals()\ncallback = unused\n',
            'LABEL = "owned"\nALIASES = [LABEL]\n',
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for setup in setups:
            with self.subTest(setup=setup):
                source = ('import types\n' + setup + 'TESTS = ["owned"]\n'
                          'first = second = vars(types.SimpleNamespace())\n'
                          'first["TESTS"] = []\nsecond["again"] = [1, 2]\n'
                          'vars(types.SimpleNamespace())\n')
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)

    def test_opaque_execution_only_restricts_the_reflection_exemption(self):
        controls = ['import pathlib\n', 'class Holder: pass\n',
                    'def noop(): pass\nnoop()\n', 'value = object()\n']
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for control in controls:
            with self.subTest(control=control):
                source = control + 'TESTS = ["owned"]\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
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

    def test_class_construction_keywords_cannot_escape_mutable_selectors(self):
        setup = ('def clear(value):\n    (value[0] if isinstance(value, tuple) else value).clear()\n'
                 'class Meta(type):\n'
                 '    def __new__(meta, name, bases, namespace, tests):\n'
                 '        clear(tests)\n        return super().__new__(meta, name, bases, namespace)\n'
                 'class Base:\n    def __init_subclass__(cls, tests): clear(tests)\n'
                 'TESTS = ["owned"]\nKEY = (TESTS,)\n')
        headers = ['metaclass=Meta, tests=TESTS', 'metaclass=Meta, **{"tests": TESTS}',
                   'metaclass=Meta, tests=(TESTS,)', 'metaclass=Meta, tests=KEY',
                   'Base, tests=TESTS', 'Base, **{"tests": TESTS}']
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for header in headers:
            with self.subTest(header=header):
                source = setup + f'class Holder({header}): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_subscription_protocols_cannot_mutate_selector_keys(self):
        setup = ('def clear(key):\n'
                 '    if isinstance(key, slice):\n'
                 '        key = next(part for part in (key.start, key.stop, key.step) if part is not None)\n'
                 '    if isinstance(key, tuple): key = key[0]\n    key.clear()\n'
                 'class Indexer:\n'
                 '    def __getitem__(self, key): clear(key)\n'
                 '    def __setitem__(self, key, value): clear(key)\n'
                 '    def __delitem__(self, key): clear(key)\n'
                 'INDEXER = Indexer()\nTESTS = ["owned"]\nKEY = (TESTS,)\n')
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for key in ['TESTS', '(TESTS,)', 'KEY', 'TESTS:', ':TESTS', '::TESTS']:
            for operation in [f'INDEXER[{key}]', f'INDEXER[{key}] = 0', f'del INDEXER[{key}]']:
                with self.subTest(operation=operation):
                    # A standalone read and scalar write RHS avoid the existing
                    # unsupported-assignment guard masking the key escape.
                    source = setup + operation + '\n'
                    actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, '[]\n')
                    (self.root / 'runner.py').write_text(source, encoding='utf-8')
                    with self.assertRaises(audit.InventoryError):
                        self.check()

    def test_immutable_class_keywords_preserve_selector_evidence(self):
        setup = ('class Meta(type):\n'
                 '    def __new__(meta, name, bases, namespace, tests):\n'
                 '        return super().__new__(meta, name, bases, namespace)\n'
                 'class Base:\n    def __init_subclass__(cls, tests): pass\n'
                 'LABEL = "owned"\nKEY = (LABEL,)\nTESTS = [LABEL]\n')
        headers = ['metaclass=Meta, tests=LABEL', 'metaclass=Meta, **{"tests": LABEL}',
                   'metaclass=Meta, tests=KEY', 'Base, tests=LABEL', 'Base, tests=KEY']
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for header in headers:
            with self.subTest(header=header):
                source = setup + f'class Holder({header}): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)

    def test_scalar_indices_and_ordinary_attributes_preserve_selectors(self):
        setup = ('class Indexer:\n'
                 '    def __getitem__(self, key): pass\n'
                 '    def __setitem__(self, key, value): pass\n'
                 '    def __delitem__(self, key): pass\n'
                 'INDEXER = Indexer()\nKEY = (0,)\nTESTS = ["owned"]\n')
        controls = ['TESTS[0]', 'TESTS[:]', 'TESTS[::1]',
                    'INDEXER.value = 1\nINDEXER.value\ndel INDEXER.value',
                    'BOX = [0]\nBOX[0] = 1\ndel BOX[0]']
        for key in ['0', 'KEY', ':1']:
            controls.extend([f'INDEXER[{key}]', f'INDEXER[{key}] = 0', f'del INDEXER[{key}]'])
        self.inventory["owners"]["live"]["selection"] = {
            "kind": "data", "file": "runner.py", "expression": "TESTS"}
        for control in controls:
            with self.subTest(control=control):
                source = setup + control + '\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)


    def test_class_construction_callbacks_cannot_hide_selector_mutations(self):
        construct = ('def construct(name, bases, namespace, **keywords):\n'
                     '    return type(name, (), namespace)\n')
        mutate = ('def mutate(*unused):\n    TESTS.clear()\n    return True\n')
        prepare = ('def prepare(name, bases):\n    TESTS.clear()\n    return {}\n')
        header = 'class Holder(TESTS[0], metaclass=construct): pass\n'
        cases = [
            ('def build(name, namespace): return type(name, (), namespace)\n'
             'def construct(name, bases, namespace):\n    result = build(name, namespace)\n'
             '    result.clear()\n    return result\n',
             'class Holder(TESTS[0], metaclass=construct):\n    def clear(): TESTS.clear()\n'),
            (construct + mutate, 'class Holder(TESTS[0], flag=mutate(), metaclass=construct): pass\n'),
            (construct + mutate, 'class Holder(TESTS[0], **{"flag": mutate()}, metaclass=construct): pass\n'),
            (construct + 'def select():\n    TESTS.clear()\n    return construct\n',
             'class Holder(TESTS[0], metaclass=select()): pass\n'),
            (construct + mutate, 'class Holder(TESTS[0], metaclass=construct): mutate()\n'),
            (construct + mutate, 'class Holder(TESTS[0], metaclass=construct):\n'
             '    class Inner:\n        mutate()\n'),
            (construct + mutate, 'class Holder(TESTS[0], metaclass=construct):\n'
             '    def method(self, value=mutate()): pass\n'),
            (construct + 'def decorate(fn):\n    TESTS.clear()\n    return fn\n',
             'class Holder(TESTS[0], metaclass=construct):\n'
             '    @decorate\n    def method(self): pass\n'),
            ('def construct(name, bases, namespace):\n    TESTS.clear()\n'
             '    return type(name, (), namespace)\n', header),
            ('def change(): TESTS.clear()\ndef construct(name, bases, namespace):\n'
             '    change()\n    return type(name, (), namespace)\n', header),
            ('def type(*args):\n    TESTS.clear()\n    return object\n' + construct, header),
            (construct + prepare + 'construct.__prepare__ = prepare\n', header),
            (construct + prepare + 'ALIAS = construct\nALIAS.__prepare__ = prepare\n', header),
            (construct + prepare + 'setattr(construct, "__prepare__", prepare)\n', header),
            (construct + prepare + 'def install(): construct.__prepare__ = prepare\ninstall()\n', header),
            (construct + 'class Mapping:\n    def keys(self):\n        TESTS.clear()\n'
             '        return ["flag"]\n    def __getitem__(self, key): return True\nMAPPING = Mapping()\n',
             'class Holder(TESTS[0], **MAPPING, metaclass=construct): pass\n'),
            ('def clear(value): ALIAS.clear()\ndef construct(name, bases, namespace):\n'
             '    clear(bases)\n    return type(name, (), namespace)\n', 'ALIAS = TESTS\n' + header),
            ('', 'def clear(value=TESTS): value.clear()\ndef construct(name, bases, namespace):\n'
             '    clear()\n    return type(name, (), namespace)\n' + header),
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for setup, suffix in cases:
            with self.subTest(setup=setup, suffix=suffix):
                source = setup + 'TESTS = ["owned"]\n' + suffix
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_proven_native_class_execution_preserves_selector_ownership(self):
        construct = ('def construct(name, bases, namespace, **keywords):\n'
                     '    return type(name, (), namespace)\n')
        header = 'class Holder(TESTS[0], metaclass=construct): pass\n'
        cases = [
            (construct, 'class Holder(TESTS[0], metaclass=construct):\n'
             '    def unused(self): TESTS.clear()\n'),
            (construct, 'class Holder(TESTS[0], metaclass=construct):\n'
             '    async def unused(self, value=1): TESTS.clear()\n'),
            ('', 'class Holder(*TESTS[:0]): pass\n'),
            ('', 'class Holder(*TESTS[:0], metaclass=type): pass\n'),
            (construct + 'OPTIONS = {"flag": True}\nALIAS = OPTIONS\n',
             'class Holder(TESTS[0], **ALIAS, metaclass=construct): pass\n'),
            (construct, 'class Holder(TESTS[0], flag=True, metaclass=construct): pass\n'),
            (construct, 'class Holder(*TESTS, **{"flag": True}, metaclass=construct): pass\n'),
            (construct, 'class Holder(TESTS[0], flag={"x": [1]}, metaclass=construct): pass\n'),
            (construct + 'ALIAS = construct\n', 'class Holder(TESTS[0], metaclass=ALIAS): pass\n'),
            (construct, 'class Holder(TESTS[0], metaclass=construct):\n    TESTS = 0\n    count = TESTS + 1\n'),
            ('def construct(name, bases, namespace):\n    copied = list(bases)\n'
             '    copied.clear()\n    return type(name, (), namespace)\n', header),
            ('def clear(value):\n    if isinstance(value, list): value.clear()\nALIAS = clear\n'
             'def construct(name, bases, namespace):\n    for base in bases: ALIAS(base)\n'
             '    return type(name, (), namespace)\n', header),
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for setup, suffix in cases:
            with self.subTest(setup=setup, suffix=suffix):
                source = setup + 'TESTS = ["owned"]\n' + suffix
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)

    def test_literal_base_elements_and_copies_preserve_selector_ownership(self):
        setup = ('def clear(value):\n'
                 '    if isinstance(value, list): value.clear()\n'
                 '    elif isinstance(value, tuple):\n'
                 '        for child in value: clear(child)\n'
                 'def construct(name, bases, namespace):\n'
                 '    for base in bases: clear(base)\n'
                 '    return type(name, (), namespace)\n')
        bases = ['*TESTS', '*ALIAS', 'TESTS[0]', 'ALIAS[-1]', 'TESTS[0][0:]',
                 'TESTS[:]', 'ALIAS[::-1]', '*TESTS[:]', '(TESTS[0],)',
                 '[ALIAS[0]]', '*[TESTS[0]]', '*[name for name in TESTS]',
                 'TESTS[:1] + TESTS[:0]', 'WRAPPED[0][0]', '*WRAPPED[0]']
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for declaration in ('["owned"]', '("owned",)'):
            for base in bases:
                with self.subTest(declaration=declaration, base=base):
                    source = (setup + f'TESTS = {declaration}\nALIAS = TESTS\nWRAPPED = (TESTS,)\n'
                              + f'class Holder({base}, metaclass=construct): pass\n')
                    actual = subprocess.run([sys.executable, '-c', source + 'print(list(TESTS))'],
                                            check=True, capture_output=True, text=True, timeout=10)
                    self.assertEqual(actual.stdout, "['owned']\n")
                    (self.root / 'runner.py').write_text(source, encoding='utf-8')
                    self.assertEqual(self.check(), 1)

    def test_computed_bases_expose_shared_mutable_children(self):
        setup = ('def clear(value):\n'
                 '    if isinstance(value, (list, tuple)):\n'
                 '        for child in list(value): clear(child)\n'
                 '        if isinstance(value, list): value.clear()\n'
                 'def construct(name, bases, namespace):\n'
                 '    for base in bases: clear(base)\n'
                 '    return type(name, (), namespace)\n'
                 'TESTS = ["owned"]\nALIAS = TESTS\nWRAPPED = (ALIAS,)\n'
                 'NESTED = [WRAPPED]\n')
        bases = ['WRAPPED[0]', '*WRAPPED', '*WRAPPED[:]', 'NESTED[0][0]',
                 '*NESTED[0]', 'NESTED[:]', '([WRAPPED[0]],)',
                 '[WRAPPED[0] for unused in [0]]', '"safe", WRAPPED[0]']
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for base in bases:
            with self.subTest(base=base):
                source = setup + f'class Holder({base}, metaclass=construct): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_unproven_base_execution_keeps_conservative_selector_effects(self):
        setup = ('def construct(name, bases, namespace):\n'
                 '    for base in bases:\n'
                 '        if isinstance(base, list): base.clear()\n'
                 '    return type(name, (), namespace)\n'
                 'def opaque(value): return (value,)\n'
                 'TESTS = ["owned"]\n')
        suffixes = [
            'def decorate():\n    TESTS.append(TESTS)\n    return lambda cls: cls\n'
            '@decorate()\nclass Holder(*TESTS, metaclass=construct): pass\n',
            'def expand():\n    TESTS.append(TESTS)\n    return "safe"\n'
            'class Holder(expand(), *TESTS, metaclass=construct): pass\n',
            'class Holder(*opaque(TESTS), metaclass=construct): pass\n',
            'class Holder(opaque(TESTS)[0], metaclass=construct): pass\n',
            'class Outer:\n    ALIAS = TESTS\n    class Holder(ALIAS, metaclass=construct): pass\n',
            'class Outer:\n    TESTS = (TESTS,)\n    class Holder(*TESTS, metaclass=construct): pass\n',
            'class Iterable:\n    def __iter__(self):\n        yield TESTS\n'
            'class Holder(*Iterable(), TESTS, metaclass=construct): pass\n',
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for suffix in suffixes:
            with self.subTest(suffix=suffix):
                source = setup + suffix
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_callable_metaclasses_cannot_mutate_positional_selector_bases(self):
        setup = ('def clear(value):\n'
                 '    if isinstance(value, tuple) or (value and isinstance(value[0], (list, tuple))):\n'
                 '        for child in value: clear(child)\n'
                 '    else: value.clear()\n'
                 'def construct(name, bases, namespace):\n'
                 '    for base in bases: clear(base)\n'
                 '    return type(name, (), namespace)\n'
                 'TESTS = ["owned"]\nALIAS = TESTS\nWRAPPED = (TESTS,)\n'
                 'NESTED = (WRAPPED,)\nLIST_WRAPPER = [TESTS]\n')
        bases = ['TESTS', 'ALIAS', '(TESTS,)', 'WRAPPED', 'NESTED', '[TESTS]',
                 'LIST_WRAPPER', '*(TESTS,)', '*WRAPPED', '*LIST_WRAPPER',
                 '*([TESTS],)', 'TESTS, ALIAS', '(), TESTS']
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for base in bases:
            with self.subTest(base=base):
                source = setup + f'class Holder({base}, metaclass=construct): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, '[]\n')
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                with self.assertRaises(audit.InventoryError):
                    self.check()

    def test_ordinary_and_immutable_class_bases_preserve_selectors(self):
        setup = ('def construct(name, bases, namespace): return type(name, (), namespace)\n'
                 'class Base: pass\n')
        controls = [
            ('TESTS = ["owned"]', 'Base'),
            ('TESTS = ["owned"]', 'object'),
            ('TESTS = ["owned"]', ''),
            ('TESTS = ("owned",)', 'TESTS, metaclass=construct'),
            ('TESTS = ("owned",)\nALIAS = TESTS', 'ALIAS, metaclass=construct'),
            ('TESTS = ("owned",)\nWRAPPED = (TESTS,)', 'WRAPPED, metaclass=construct'),
            ('TESTS = ("owned",)', '*(TESTS,), metaclass=construct'),
            ('TESTS = ["owned"]', '("local",), metaclass=construct'),
            ('TESTS = ["owned"]\nLABEL = "local"', 'LABEL, metaclass=construct'),
        ]
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for selectors, bases in controls:
            with self.subTest(selectors=selectors, bases=bases):
                source = setup + selectors + '\n' + f'class Holder({bases}): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(list(TESTS))'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)

    def test_uncalled_class_construction_does_not_escape_module_selectors(self):
        setup = ('def construct(name, bases, namespace):\n'
                 '    bases[0].clear()\n    return type(name, (), namespace)\n'
                 'TESTS = ["owned"]\n')
        self.inventory['owners']['live']['selection'] = {
            'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'}
        for definition in ('def unused():', 'async def unused():'):
            with self.subTest(definition=definition):
                source = setup + definition + '\n    class Holder(TESTS, metaclass=construct): pass\n'
                actual = subprocess.run([sys.executable, '-c', source + 'print(TESTS)'],
                                        check=True, capture_output=True, text=True, timeout=10)
                self.assertEqual(actual.stdout, "['owned']\n")
                (self.root / 'runner.py').write_text(source, encoding='utf-8')
                self.assertEqual(self.check(), 1)



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
            # Compare skip behavior with libtest for either position of the
            # positive filter; exact mode applies to exclusions as well.
            for before, filters in [(["owned"], []), ([], ["owned"])]:
                for mode, skips, expected in [
                    ([], ["own"], set()),
                    (["--exact"], ["own"], {"owned"}),
                    (["--exact"], ["owned"], set()),
                    ([], ["missing"], {"owned"}),
                    (["--exact"], ["own", "other"], {"owned"}),
                    (["--exact"], ["missing", "owned"], set()),
                    ([], [""], set()),
                    (["--exact"], [""], {"owned"}),
                ]:
                    skip_args = [arg for skip in skips for arg in ("--skip", skip)]
                    cases.append((before, ["--ignored", *filters, *mode, *skip_args], expected))
            cases.extend([
                (["owned"], ["--ignored", "other", "--exact", "--skip", "own", "--skip", "other"], {"owned"}),
                (["owned"], ["--ignored", "other", "--skip", "own", "--skip", "other"], set()),
            ])
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
