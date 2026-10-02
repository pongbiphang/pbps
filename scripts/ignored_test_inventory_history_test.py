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


class HistoricalCases(unittest.TestCase):
    def test_every_retired_property_has_preserved_program_inputs(self):
        history = json.loads((Path(__file__).parent / 'fixtures/closed-python-history.json').read_text())
        cases = history['cases']
        self.assertEqual({case['origin'] for case in cases}, set(history['migrated_methods']))
        self.assertEqual(len({case['id'] for case in cases}), len(cases))
        self.assertTrue(all(case['format'] and case['source'] and case['runtime'] for case in cases))

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
                if 'error' in expected:
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
                if case['diagnostic'] is None:
                    self.assertEqual(audit.validate(root, owner.inventory, owner.targets, platform), 1)
                else:
                    with self.assertRaises(audit.InventoryError) as raised:
                        audit.validate(root, owner.inventory, owner.targets, platform)
                    self.assertEqual(str(raised.exception), case['diagnostic'])
            finally:
                owner.doCleanups()

        # Parallel child interpreters have isolated temporary directories; the
        # checker itself neither imports nor executes these fixture programs.
        with ThreadPoolExecutor(max_workers=4) as pool:
            tasks = [(case, pool.submit(exercise, case)) for case in history['cases']]
            for case, task in tasks:
                with self.subTest(case=case['id'], property=case['origin']):
                    task.result()


if __name__ == '__main__':
    unittest.main()
