"""Preserved program inputs, replacing retired evaluator APIs (DEC-1413.1)."""

import ast
from concurrent.futures import ThreadPoolExecutor
import json
from pathlib import Path
import subprocess
import sys
import unittest

import ignored_test_inventory as audit
import ignored_test_inventory_closed_test as closed


def python_range(case):
    """An explicit syntax window, never a fallback for unexpected failures."""
    support = case.get('python', {})
    if ('python' in case and (not isinstance(support, dict)
            or set(support) - {'minimum', 'before', 'reason'}
            or not {'minimum', 'before'}.intersection(support)
            or not isinstance(support.get('reason'), str) or not support['reason'].strip())):
        raise ValueError('invalid Python support range')
    minimum, before = support.get('minimum', [3, 11]), support.get('before')
    if minimum is None or ('before' in support and before is None):
        raise ValueError('invalid Python support range')
    for version in (minimum, before):
        if version is not None and (not isinstance(version, (list, tuple)) or len(version) != 2
                or any(type(part) is not int or part < 0 for part in version)):
            raise ValueError('invalid Python support range')
    minimum = tuple(minimum)
    before = tuple(before) if before is not None else None
    if minimum < (3, 11) or (before is not None and before <= minimum):
        raise ValueError('invalid Python support range')
    return minimum, before


def supports_python(case, version=None):
    minimum, before = python_range(case)
    version = sys.version_info[:2] if version is None else version
    if version < minimum:
        return False
    if before is not None and version >= before:
        return False
    return True


class HistoricalCases(unittest.TestCase):
    def test_every_retired_property_has_preserved_program_inputs(self):
        history = json.loads((Path(__file__).parent / 'fixtures/closed-python-history.json').read_text())
        cases = history['cases']
        self.assertEqual({case['origin'] for case in cases}, set(history['migrated_methods']))
        self.assertEqual(len({case['id'] for case in cases}), len(cases))
        self.assertTrue(all(case['format'] and case['source'] and case['runtime'] for case in cases))

    def test_syntax_windows_keep_both_boundaries_and_ordinary_programs(self):
        for version in ((3, 11), (3, 12), (3, 13), (3, 14)):
            self.assertTrue(supports_python({}, version))
            self.assertEqual(supports_python({'python': {'minimum': [3, 12], 'reason': 'type alias'}},
                                            version), version >= (3, 12))
            self.assertEqual(supports_python({'python': {'before': [3, 14], 'reason': 'annotation'}},
                                            version), version < (3, 14))
        for support in ({}, {'minimum': [3, 12]}, {'minimum': [3, True], 'reason': 'invalid'},
                        {'minimum': None, 'reason': 'missing'}, {'before': None, 'reason': 'missing'},
                        {'minimum': [3, 12], 'before': [3, 12], 'reason': 'empty'},
                        {'minimum': [3, 10], 'reason': 'below the documented minimum'},
                        {'before': [3, 14], 'reason': ' ', 'skip': True}):
            with self.subTest(support=support), self.assertRaises(ValueError):
                python_range({'python': support})

    def test_historical_programs_keep_their_runtime_and_complete_owner_dispositions(self):
        history = json.loads((Path(__file__).parent / 'fixtures/closed-python-history.json').read_text())
        self.exercise_programs(history)

    def exercise_programs(self, history):

        def exercise(case):
            owner = closed.ClosedFixtures()
            owner.setUp()
            try:
                root = owner.root
                source = case['source'].rstrip() + '\n' + case['adapter']
                for name, body in case['helpers'].items():
                    path = root / name
                    path.parent.mkdir(parents=True, exist_ok=True)
                    path.write_text(body, encoding='utf-8')
                observed = root / 'observed.py'
                trailer = ('\nprint(repr(' + case['expression'] + '))\n'
                           if case['expression'] is not None else '')
                observed.write_text(source + trailer, encoding='utf-8')
                child = subprocess.run([sys.executable, '-B', str(observed)], cwd=root,
                                       capture_output=True, text=True, timeout=10)
                expected = case['runtime']
                supported = supports_python(case)
                if not supported:
                    # Keep the original program as an explicit compatibility
                    # control. A broad skip or unexpected runtime error cannot
                    # stand in for its historical execution (DEC-1428.1).
                    self.assertNotEqual(child.returncode, 0, 'version-ineligible syntax executed')
                    self.assertEqual(child.stderr.splitlines()[-1].split(':')[0], 'SyntaxError')
                elif 'error' in expected:
                    # A failed execution is retained as a failure control. It
                    # supplies no successful empty-selector evidence.
                    self.assertNotEqual(child.returncode, 0)
                    self.assertEqual(child.stderr.splitlines()[-1].split(':')[0], expected['error'])
                elif 'stdout' in expected:
                    self.assertEqual(child.returncode, 0, child.stderr)
                    self.assertEqual(child.stdout, expected['stdout'])
                else:
                    self.assertEqual(child.returncode, 0, child.stderr)
                    value = ast.literal_eval(child.stdout.splitlines()[-1])
                    wanted = tuple(expected['value']) if expected['tuple'] else expected['value']
                    self.assertEqual(value, wanted)
                    if value == []:
                        self.assertIsNotNone(case['diagnostic'])
                filename = 'runner.py'
                if 'configuration' in case:
                    config = history['configurations'][case['configuration']]
                    owner.inventory = config['inventory']
                    owner.targets = {tuple(key): {name: set(values) for name, values in target.items()}
                                     for key, target in config['targets']}
                    (root / '.github/workflows/ci.yml').write_text(config['workflow'], encoding='utf-8')
                    filename = config['filename']
                    platform = config['platform']
                else:
                    owner.inventory['owners']['live']['witnesses'].pop()
                    if case.get('entry'):
                        del owner.inventory['owners']['live']['selection']
                    platform = 'linux'
                (root / filename).write_text(source, encoding='utf-8')
                minimum, _ = python_range(case)
                if not supported and sys.version_info[:2] < minimum:
                    with self.assertRaisesRegex(audit.InventoryError,
                                                r'runner.py:\d+: invalid Python fixture: '):
                        audit.validate(root, owner.inventory, owner.targets, platform)
                elif case['diagnostic'] is None:
                    self.assertEqual(audit.validate(root, owner.inventory, owner.targets, platform), 1)
                else:
                    with self.assertRaises(audit.InventoryError) as raised:
                        audit.validate(root, owner.inventory, owner.targets, platform)
                    self.assertEqual(str(raised.exception), case['diagnostic'])
                # Above the annotation boundary Python still builds the AST;
                # retain the original owner diagnostic even though compilation
                # fails. Only completed runtime AND owner checks earn a receipt.
                return 'executed' if supported else 'unsupported-syntax'
            finally:
                owner.doCleanups()

        # Parallel child interpreters have isolated temporary directories; the
        # checker itself neither imports nor executes these fixture programs.
        with ThreadPoolExecutor(max_workers=4) as pool:
            tasks = [(case, pool.submit(exercise, case)) for case in history['cases']]
            outcomes = {}
            for case, task in tasks:
                with self.subTest(case=case['id'], property=case['origin']):
                    outcomes[case['id']] = task.result()
        self.assertEqual(set(outcomes), {case['id'] for case in history['cases']},
                         'every preserved program needs an actual-interpreter and owner receipt')
        # Independent range comparison detects dropped tasks and broad skips;
        # eligibility itself cannot manufacture execution receipts.
        expected = {case['id'] for case in history['cases']
                    if python_range(case)[0] <= sys.version_info[:2]
                    and (python_range(case)[1] is None or sys.version_info[:2] < python_range(case)[1])}
        self.assertEqual({name for name, outcome in outcomes.items() if outcome == 'executed'}, expected,
                         'every version-eligible program must actually execute')
        self.assertEqual({name for name, outcome in outcomes.items() if outcome == 'unsupported-syntax'},
                         set(outcomes) - expected)
        return outcomes


if __name__ == '__main__':
    unittest.main()
