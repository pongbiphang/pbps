#!/usr/bin/env python3
"""Actual-Python and independent rule-removal controls for DEC-1413.1."""

import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

import ignored_test_inventory as audit


KEY = ('demo', 'lib', 'demo')
ENTRY = 'if __name__ == "__main__":\n    main()\n'
MAIN = ('def main():\n    global RAN\n    RAN = True\n'
        '    for test in TESTS:\n        pass\n')


def fixture(body='', declaration='TESTS = ["owned"]\n', *, main=MAIN, entry=ENTRY):
    return 'RAN = False\n' + declaration + body + main + entry


# The expected restriction and runtime outcome are explicit, not inferred from
# checker results. Removing a rule must admit this complete execution owner.
RULE_CASES = {
    'declarations': (fixture('TESTS = []\n'), 'unique module literal declaration TESTS', [], True, {}),
    'comprehensions': (fixture(declaration='TESTS = [name for name in ["owned"] if False]\n'),
                       'single synchronous unfiltered selector comprehension', [], True, {}),
    'bindings': (fixture('if True:\n    TESTS = []\n'), 'protected binding TESTS', [], True, {}),
    'list_reads': (fixture('TESTS.clear()\n'), 'list selector outside iteration TESTS', [], True, {}),
    'reflection_names': (fixture('namespace = globals()\nnamespace["TESTS"] = []\n'),
                         'reflective spelling globals', [], True, {}),
    'reflection_imports': (fixture('import builtins as defaults\nnamespace = getattr(defaults, "globals")()\nnamespace["TESTS"] = []\n'),
                           'reflection import builtins', [], True, {}),
    'wildcards': (fixture('from helper import *\n'), 'wildcard import', ['owned'], False,
                  {'helper.py': '__all__ = ["__name__"]\n__name__ = "imported"\n'}),
    'sys_access': (fixture('import sys as process\nnamespace = getattr(process._getframe(), "f_globals")\nnamespace["TESTS"] = []\n'),
                   'reflective sys attribute _getframe', [], True, {}),
    'functions': (fixture('def suppress(function):\n    return lambda: None\n', main='@suppress\n' + MAIN),
                  'unique undecorated module function main', ['owned'], False, {}),
    # This unsupported guard really runs: entry-policy sensitivity is distinct
    # from claiming that every noncanonical guard loses an invocation.
    'entry': (fixture(entry='if True:\n    main()\n'), 'final canonical main entry', ['owned'], True, {}),
}


class ClosedFixtures(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        (self.root / '.github/workflows').mkdir(parents=True)
        (self.root / '.github/workflows/ci.yml').write_text(
            'jobs:\n  live:\n    runs-on: ubuntu-latest\n    steps:\n'
            '      - run: python3 runner.py\n', encoding='utf-8')
        self.inventory = {
            'version': 1,
            'owners': {'live': {'platform': 'linux', 'fixture': 'Synthetic source oracle',
                               'job': 'live', 'script': 'runner.py',
                               'selection': {'kind': 'data', 'file': 'runner.py', 'expression': 'TESTS'},
                               'witnesses': [{'file': 'runner.py', 'language': 'python',
                                              'scope': '<module>', 'requires': ['main()']},
                                             {'file': 'runner.py', 'language': 'python',
                                              'scope': 'main', 'requires': ['for test in TESTS:']}]}},
            'groups': [{'target': list(KEY), 'owner': 'live', 'validate_on': 'linux',
                        'ignored_on': ['linux', 'win32', 'darwin'], 'cases': ['owned']}],
        }
        self.targets = {KEY: {'all': {'owned'}, 'ignored': {'owned'}}}

    def check(self, source, helpers=None):
        (self.root / 'runner.py').write_text(source, encoding='utf-8')
        for name, body in (helpers or {}).items():
            (self.root / name).write_text(body, encoding='utf-8')
        return audit.validate(self.root, self.inventory, self.targets, 'linux')

    def actual(self, source, helpers=None):
        for name, body in (helpers or {}).items():
            (self.root / name).write_text(body, encoding='utf-8')
        observed = self.root / 'observed.py'
        observed.write_text(source + '\nimport json\nprint(json.dumps([TESTS, RAN]))\n', encoding='utf-8')
        child = subprocess.run([sys.executable, '-B', str(observed)], cwd=self.root,
                               capture_output=True, text=True, check=True, timeout=10)
        return json.loads(child.stdout.splitlines()[-1])

    def test_every_operational_rule_has_an_independent_complete_owner_counterfactual(self):
        self.assertEqual(set(RULE_CASES), set(audit.RULES))
        for name, (source, construct, selected, ran, helpers) in RULE_CASES.items():
            with self.subTest(rule=name):
                self.assertEqual(self.actual(source, helpers), [selected, ran])
                with self.assertRaises(audit.InventoryError) as raised:
                    self.check(source, helpers)
                self.assertRegex(str(raised.exception), r'runner.py:\d+: outside closed fixture form: ')
                self.assertTrue(str(raised.exception).endswith(construct), str(raised.exception))
                remaining = {key: value for key, value in audit.RULES.items() if key != name}
                with patch.object(audit, 'RULES', remaining):
                    self.assertEqual(self.check(source, helpers), 1)
                with self.assertRaises(audit.InventoryError):
                    self.check(source, helpers)

    def test_qualified_and_imported_list_reads_cannot_bypass_iteration_contexts(self):
        for body in ('import __main__ as runner\nrunner.TESTS.clear()\n',
                     'from __main__ import TESTS as alias\nalias.clear()\n'):
            with self.subTest(body=body):
                source = fixture(body)
                self.assertEqual(self.actual(source), [[], True])
                with self.assertRaisesRegex(audit.InventoryError, 'list selector outside iteration TESTS'):
                    self.check(source)
                with patch.object(audit, 'RULES', {key: value for key, value in audit.RULES.items() if key != 'list_reads'}):
                    self.assertEqual(self.check(source), 1)

    def test_reference_string_fields_cannot_hide_mutable_list_or_namespace_reads(self):
        cases = [
            ('import __main__ as runner\nfrom types import ModuleType\nmatch runner:\n'
             '    case ModuleType(TESTS=alias):\n        alias.clear()\n',
             'list_reads', 'list selector outside iteration TESTS'),
            ('import __main__ as runner\nfrom types import ModuleType\nmatch runner:\n'
             '    case ModuleType(__dict__=namespace):\n        namespace["TESTS"] = []\n',
             'reflection_names', 'reflective spelling __dict__'),
            ('from __main__ import __dict__ as namespace\nnamespace["TESTS"] = []\n',
             'reflection_names', 'reflective spelling __dict__'),
        ]
        for body, rule, construct in cases:
            with self.subTest(body=body):
                source = fixture(body)
                self.assertEqual(self.actual(source), [[], True])
                with self.assertRaisesRegex(audit.InventoryError, construct):
                    self.check(source)
                with patch.object(audit, 'RULES', {key: value for key, value in audit.RULES.items() if key != rule}):
                    self.assertEqual(self.check(source), 1)
                with self.assertRaisesRegex(audit.InventoryError, construct):
                    self.check(source)

    def test_module_annotations_keep_real_mutation_and_versioned_failure_controls(self):
        bodies = (
            'annotation: TESTS\n__annotations__["annotation"].clear()\n',
            'annotation: TESTS\nimport __main__ as runner\n'
            'runner.__annotations__["annotation"].clear()\n',
        )
        for index, body in enumerate(bodies):
            with self.subTest(body=body):
                source = fixture(body)
                # Runtime annotation evaluation changed in 3.14. The owner
                # refusal is syntactic and must never inherit that branch
                # or lose this protected list read (DEC-1262.1).
                diagnostic = 'runner.py:3: outside closed fixture form: list selector outside iteration TESTS'
                with self.assertRaises(audit.InventoryError) as raised:
                    self.check(source)
                self.assertEqual(str(raised.exception), diagnostic)
                with patch.object(audit, 'RULES', {key: value for key, value in audit.RULES.items()
                                                  if key != 'list_reads'}):
                    self.assertEqual(self.check(source), 1)
                with self.assertRaises(audit.InventoryError):
                    self.check(source)
                if index == 0 and sys.version_info >= (3, 14):
                    # Preserve the exact bare-name regression as a failure
                    # control; it supplies no successful mutation evidence.
                    with self.assertRaises(subprocess.CalledProcessError) as failure:
                        self.actual(source)
                    self.assertEqual(failure.exception.returncode, 1)
                    self.assertEqual(failure.exception.stdout, '')
                    self.assertRegex(failure.exception.stderr.splitlines()[-1],
                                     r"^NameError: name '__annotations__' is not defined(?:\.|$)")
                else:
                    # Module attribute access forces lazy evaluation on 3.14
                    # and still exposes the actual mutable selector on 3.12.
                    self.assertEqual(self.actual(source), [[], True])

    def test_unrelated_native_expressions_and_annotations_are_not_interpreted(self):
        for body in ('value = ~0\n', 'value = {1}\n', 'value: int\n',
                     'from __future__ import annotations\n',
                     'type Alias = int\n' if sys.version_info >= (3, 12) else 'Alias = int\n',
                     'global ENGINE, DOCKER_SOCKET\nENGINE = None\nDOCKER_SOCKET = None\n'):
            with self.subTest(body=body):
                source = fixture(body)
                if body.startswith('from __future__'):
                    source = body + fixture('value: int\n')
                self.assertEqual(self.actual(source), [['owned'], True])
                self.assertEqual(self.check(source), 1)

    def test_one_literal_grammar_handles_prefixes_tuples_constants_and_compound_lists(self):
        declarations = [
            'TESTS = ["owned"]\n',
            'TESTS = ("owned",)\n',
            'PREFIX = "ow"\nTESTS = [PREFIX + name for name in ["ned"]] + []\n',
            'TESTS = [name for name in ("owned",)]\n',
            'TESTS = ["ow" + "ned"]\n',
        ]
        for declaration in declarations:
            with self.subTest(declaration=declaration):
                self.assertEqual(self.check(fixture(declaration=declaration)), 1)
        for expression in ('"owned"', '"ow" + "ned"'):
            source = fixture(declaration='TEST = ' + expression + '\n', main=MAIN.replace('in TESTS', 'in [TEST]'))
            self.inventory['owners']['live']['selection']['expression'] = 'TEST'
            self.inventory['owners']['live']['witnesses'][1]['requires'] = ['for test in [TEST]:']
            self.assertEqual(self.check(source), 1)

    def test_calls_use_the_same_protected_literal_constants(self):
        source = ('TEST = "owned"\ndef selected(name):\n    pass\ndef main():\n'
                  '    selected(TEST)\n' + ENTRY)
        owner = self.inventory['owners']['live']
        owner['selection'] = {'kind': 'calls', 'file': 'runner.py', 'scope': 'main',
                              'callee': 'selected', 'argument': 0}
        owner['witnesses'][1]['requires'] = ['selected(TEST)']
        self.assertEqual(self.check(source), 1)
        with self.assertRaisesRegex(audit.InventoryError, 'protected binding TEST'):
            self.check(source.replace('def selected(name):', 'def selected(TEST):'))

    def test_non_string_call_arguments_report_the_original_source_line(self):
        owner = self.inventory['owners']['live']
        owner['selection'] = {'kind': 'calls', 'file': 'runner.py', 'scope': 'main',
                              'callee': 'selected', 'argument': 0}
        owner['witnesses'][1]['requires'] = ['selected(']
        for argument in ('[]', '()', 'None', '42'):
            with self.subTest(argument=argument):
                source = ('def selected(name):\n    pass\n\ndef main():\n'
                          '    selected(' + argument + ')\n' + ENTRY)
                with self.assertRaisesRegex(audit.InventoryError,
                                            r'runner.py:5:.*argument must be a string'):
                    self.check(source)
        source = 'def selected(): pass\n\ndef main():\n    selected()\n' + ENTRY
        with self.assertRaisesRegex(audit.InventoryError, r'runner.py:4:.*missing selected case argument'):
            self.check(source)

    def test_statement_boundaries_have_actual_python_and_full_owner_counterfactuals(self):
        owner = self.inventory['owners']['live']
        owner['witnesses'][1]['requires'].append('run()')
        run = 'def run():\n    global RAN\n    RAN = True\n'
        main = 'def main():\n    for test in TESTS:\n        pass\n    '
        original_tokens = audit.python_tokens
        for separator in ('\n    ', '; '):
            with self.subTest(separator=separator):
                source = fixture(run, main=main + 'run' + separator + '()\n')
                self.assertEqual(self.actual(source), [['owned'], False])
                with self.assertRaisesRegex(audit.InventoryError, 'removed invocation.*run'):
                    self.check(source)
                def without_boundaries(text):
                    return [token for token in original_tokens(text)
                            if token not in (('statement',), ';')]
                with patch.object(audit, 'python_tokens', without_boundaries):
                    self.assertEqual(self.check(source), 1)
                with self.assertRaisesRegex(audit.InventoryError, 'removed invocation.*run'):
                    self.check(source)

    def test_selector_subscriptions_and_opaque_calls_are_outside_literal_grammar(self):
        for declaration in ('TESTS = ["owned"][0:]\n', 'TESTS = list(["owned"])\n',
                            'TESTS = sorted(["owned"])\n', 'A = B\nB = A\nTESTS = [A]\n'):
            with self.subTest(declaration=declaration):
                with self.assertRaises(audit.InventoryError):
                    self.check(fixture(declaration=declaration))

    def test_protected_bindings_cover_non_name_ast_fields(self):
        for body in ('def TESTS(): pass\n', 'class TESTS: pass\n',
                     'def unused(TESTS): pass\n', 'import json as TESTS\n',
                     'from json import loads as TESTS\n',
                     'try:\n    pass\nexcept Exception as TESTS:\n    pass\n',
                     'match "value":\n    case TESTS:\n        pass\n',
                     'def unused():\n    global TESTS\n',
                     'class Local:\n    TESTS: int\n'):
            with self.subTest(body=body):
                with self.assertRaisesRegex(audit.InventoryError, 'protected binding TESTS'):
                    self.check(fixture(body))

    def test_type_parameter_bindings_keep_actual_syntax_and_rule_removal_controls(self):
        for parameter in ('TESTS', '*TESTS', '**TESTS'):
            with self.subTest(parameter=parameter):
                source = fixture('def unused[' + parameter + '](): pass\n')
                if sys.version_info[:2] < (3, 12):
                    observed = self.root / 'observed.py'
                    observed.write_text(source, encoding='utf-8')
                    child = subprocess.run([sys.executable, '-B', str(observed)], cwd=self.root,
                                           capture_output=True, text=True, timeout=10)
                    self.assertNotEqual(child.returncode, 0)
                    self.assertEqual(child.stderr.splitlines()[-1].split(':')[0], 'SyntaxError')
                    with self.assertRaisesRegex(audit.InventoryError,
                                                r'runner.py:3: invalid Python fixture: '):
                        self.check(source)
                else:
                    self.assertEqual(self.actual(source), [['owned'], True])
                    with self.assertRaisesRegex(audit.InventoryError,
                                                r'runner.py:3:.*protected binding TESTS'):
                        self.check(source)
                    with patch.object(audit, 'RULES', {key: value for key, value in audit.RULES.items()
                                                     if key != 'bindings'}):
                        self.assertEqual(self.check(source), 1)

    def test_reflection_is_checked_as_ast_nodes_and_never_as_string_contents(self):
        self.assertEqual(self.check(fixture('message = "globals gc builtins __dict__"\n')), 1)
        for body, construct in [('import gc\n', 'reflection import gc'),
                                ('import builtins as defaults\n', 'reflection import builtins'),
                                ('from sys import modules as modules_copy\n', 'reflective sys import'),
                                ('def unused():\n    value = globals()\n', 'reflective spelling globals'),
                                ('class Local:\n    __name__ = "local"\n', 'protected binding __name__')]:
            with self.subTest(body=body):
                with self.assertRaisesRegex(audit.InventoryError, construct):
                    self.check(fixture(body))

    def test_system_exit_entry_and_trusted_native_helpers_remain_supported(self):
        self.assertEqual(self.check(fixture('import ctypes\n', entry=ENTRY.replace('main()', 'raise SystemExit(main())'))), 1)


class StatementBoundaries(unittest.TestCase):
    def test_split_expressions_never_manufacture_call_witnesses(self):
        for separator in ('\n', ';'):
            source = 'def main():\n    run' + separator + ('    ' if separator == '\n' else '') + '()\n'
            self.assertFalse(audit.contains(audit.python_tokens(audit.python_scope(source, 'main')),
                                            audit.python_tokens('run()')))
        for source in ('run(\n)\n', 'run\\\n()\n', 'raise SystemExit(run())\n', 'run() # comment\n'):
            self.assertTrue(audit.contains(audit.python_tokens(source), audit.python_tokens('run()')))
        self.assertTrue(audit.contains(audit.python_tokens('run(command, check=True)'), audit.python_tokens('run(command,')))


if __name__ == '__main__':
    unittest.main()
