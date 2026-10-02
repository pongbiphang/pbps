"""Issue-to-program dispositions for the unified fixture contract."""

import json
from pathlib import Path
import unittest

import ignored_test_inventory_history_test as history_tests


class AbsorbedIssues(unittest.TestCase):
    def test_version_windows_identify_exactly_the_preserved_incompatible_programs(self):
        history = json.loads((Path(__file__).parent / 'fixtures/closed-python-obligations.json').read_text())
        aliases = {(1308, f'{use}-lazy-{call}-selector-{position}')
                   for use in ('unused', 'evaluated')
                   for call in ('globals()', 'inspect()', 'defaults.globals()')
                   for position in ('before', 'after')}
        aliases.add((1308, 'ordinary-source-without-reflective-spelling'))
        annotations = {(issue, variant) for issue in (1233, 1330)
                       for variant in ('definition-order', 'safe-definition')}
        tagged = {(case['issue'], case['variant']) for case in history['cases'] if 'python' in case}
        self.assertEqual(tagged, aliases | annotations)
        for case in history['cases']:
            key = (case['issue'], case['variant'])
            expected = ((3, 12), None) if key in aliases else ((3, 11), (3, 14)) if key in annotations else ((3, 11), None)
            with self.subTest(issue=key[0], variant=key[1]):
                self.assertEqual(history_tests.python_range(case), expected)

    def test_every_listed_issue_has_an_executable_construct_and_line_control(self):
        path = Path(__file__).parent / 'fixtures/closed-python-obligations.json'
        history = json.loads(path.read_text())
        cases = history['cases']
        self.assertEqual({case['issue'] for case in cases}, set(history['issues']))
        for index, case in enumerate(cases):
            case['id'] = f"issue-{case['issue']}-{index}"
            case['origin'] = case['variant']
            self.assertTrue(case['source'] and case['provenance'])
            if case.get('allowed'):
                self.assertIsNone(case['diagnostic'])
            elif case['issue'] == 1231:
                self.assertIn('removed invocation', case['diagnostic'])
            else:
                self.assertRegex(case['diagnostic'], r'runner.py:\d+: outside closed fixture form: ')
        # Reuse the complete owner/actual-interpreter harness, with explicit
        # issue inputs instead of retaining another evaluator-specific suite.
        history_tests.HistoricalCases.exercise_programs(self, history)


if __name__ == '__main__':
    unittest.main()
