#!/usr/bin/env python3
"""Tests for scripts/check-decisions.py. Run: python3 scripts/check_decisions_test.py"""

import importlib.util
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_decisions", Path(__file__).with_name("check-decisions.py")
)
cd = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cd)

INDEX = """# Decisions

| # | Topic | Decision |
| ---: | --- | --- |
| 1 | [identity](decisions/identity.md#decision-1) | One |
| 2 | [ledger](decisions/ledger.md#decision-2) | Two |
| 3 | [identity](decisions/identity.md#decision-3) | Three |
| 4 | [ledger](decisions/ledger.md#decision-4) | Four |
"""

IDENTITY = """# Identity

<a id="decision-1"></a>

1. **One.** Body
   continued.

<a id="decision-3"></a>

3. **Three.** Body.
"""

LEDGER = """# Ledger

<a id="decision-2"></a>

2. **Two.** Body.

<a id="decision-4"></a>

4. **Four.** Body.
"""


def new_entry(issue, k):
    return f'\n<a id="dec-{issue}-{k}"></a>\n\n**DEC-{issue}.{k}. Title.** Body.\n'


def run(topics=None, cited=None, index=INDEX):
    files = {"docs/decisions/identity.md": IDENTITY, "docs/decisions/ledger.md": LEDGER}
    files.update(topics or {})
    # The fixture's sequence closes at 4, the last number its index lists.
    return cd.check(files, index, cited or {}, closed=4)


class TheRecord(unittest.TestCase):
    def test_a_record_as_split_is_clean(self):
        self.assertEqual(run(cited={"src/a.rs": "// DECISIONS 1, DECISIONS 4"}), [])

    def test_entries_named_after_their_issues_in_two_files_are_clean(self):
        topics = {
            "docs/decisions/identity.md": IDENTITY + new_entry(737, 1),
            "docs/decisions/ledger.md": LEDGER + new_entry(740, 1) + new_entry(740, 2),
        }
        cited = {"src/a.rs": "// DEC-737.1 and DEC-740.2"}
        self.assertEqual(run(topics, cited), [])

    def test_one_identifier_in_two_files_is_refused(self):
        topics = {
            "docs/decisions/identity.md": IDENTITY + new_entry(737, 1),
            "docs/decisions/ledger.md": LEDGER + new_entry(737, 1),
        }
        errors = run(topics)
        self.assertEqual(len(errors), 1)
        self.assertIn("DEC-737.1 is already", errors[0])

    def test_the_next_sequential_number_is_refused_even_alone(self):
        stale = LEDGER + '\n<a id="decision-5"></a>\n\n5. **Five.** Body.\n'
        errors = run({"docs/decisions/ledger.md": stale})
        self.assertEqual(len(errors), 1)
        self.assertIn("5 is a new number in the closed sequence", errors[0])

    def test_the_next_number_is_refused_even_with_an_index_row(self):
        stale = LEDGER + '\n<a id="decision-5"></a>\n\n5. **Five.** Body.\n'
        index = INDEX + "| 5 | [ledger](decisions/ledger.md#decision-5) | Five |\n"
        errors = run({"docs/decisions/ledger.md": stale}, index=index)
        self.assertIn("5 is a new number in the closed sequence", " ".join(errors))
        self.assertIn("row 5 extends the closed sequence, which ends at 4", " ".join(errors))

    def test_a_number_taken_by_two_branches_in_two_files_is_refused(self):
        a = LEDGER + '\n<a id="decision-5"></a>\n\n5. **Five.** Body.\n'
        b = IDENTITY + '\n<a id="decision-5"></a>\n\n5. **Five, again.** Body.\n'
        errors = run({"docs/decisions/ledger.md": a, "docs/decisions/identity.md": b})
        self.assertTrue(any("5 is already" in e for e in errors), errors)

    def test_an_entry_without_its_anchor_is_refused(self):
        bare = IDENTITY + "\n**DEC-737.1. Title.** Body.\n"
        errors = run({"docs/decisions/identity.md": bare})
        self.assertEqual(len(errors), 1)
        self.assertIn('needs `<a id="dec-737-1"></a>`', errors[0])

    def test_an_anchor_naming_another_entry_is_refused(self):
        wrong = IDENTITY.replace('id="decision-1"', 'id="decision-3"')
        errors = run({"docs/decisions/identity.md": wrong})
        self.assertEqual(len(errors), 1)
        self.assertIn('needs `<a id="decision-1"></a>`', errors[0])

    def test_an_entry_moved_to_a_file_the_index_does_not_name_is_refused(self):
        errors = run(
            {
                "docs/decisions/identity.md": IDENTITY + LEDGER.split("\n", 2)[2],
                "docs/decisions/ledger.md": "# Ledger\n",
            }
        )
        self.assertTrue(any("places 2 in docs/decisions/ledger.md" in e for e in errors), errors)

    def test_an_indexed_entry_that_is_gone_is_refused(self):
        errors = run({"docs/decisions/ledger.md": LEDGER.split("<a id=\"decision-4\">")[0]})
        self.assertTrue(any("4 is indexed but no entry" in e for e in errors), errors)

    def test_a_citation_of_no_entry_is_refused_in_both_forms(self):
        cited = {"src/a.rs": "// DECISIONS 9\n// DEC-800.1"}
        errors = run(cited=cited)
        self.assertEqual(
            errors,
            ["src/a.rs:1: DECISIONS 9 names no entry", "src/a.rs:2: DEC-800.1 names no entry"],
        )

    # #848: the history is fixed, not read off the index.

    def test_deleting_an_entry_and_its_index_row_is_refused(self):
        ledger = LEDGER.split('<a id="decision-4">')[0]
        index = INDEX.replace("| 4 | [ledger](decisions/ledger.md#decision-4) | Four |\n", "")
        errors = run({"docs/decisions/ledger.md": ledger}, index=index)
        self.assertEqual(
            errors, ["docs/DECISIONS.md: 4 has neither an entry nor an index row; it was lost"]
        )

    def test_an_entry_or_a_row_alone_going_missing_is_refused(self):
        unindexed = INDEX.replace("| 4 | [ledger](decisions/ledger.md#decision-4) | Four |\n", "")
        self.assertEqual(
            run(index=unindexed), ["docs/decisions/ledger.md:9: 4 has no row in docs/DECISIONS.md"]
        )
        gone = LEDGER.split('<a id="decision-4">')[0]
        errors = run({"docs/decisions/ledger.md": gone})
        self.assertEqual(
            errors, ["docs/DECISIONS.md: 4 is indexed but no entry in docs/decisions/ledger.md has it"]
        )

    def test_a_jointly_deleted_number_is_not_a_skipped_citation(self):
        ledger = LEDGER.split('<a id="decision-4">')[0]
        index = INDEX.replace("| 4 | [ledger](decisions/ledger.md#decision-4) | Four |\n", "")
        errors = run({"docs/decisions/ledger.md": ledger}, {"src/a.rs": "// DECISIONS 4"}, index)
        self.assertIn("src/a.rs:1: DECISIONS 4 names no entry", errors)

    def test_only_the_four_historical_holes_can_be_absent(self):
        # The real closed sequence, every number present except the four the
        # test names itself rather than reading from the checker.
        holes = {523, 524, 525, 526}
        used = [n for n in range(1, 544) if n not in holes]

        def record(numbers):
            topic = "# All\n" + "".join(
                f'\n<a id="decision-{n}"></a>\n\n{n}. **N{n}.** Body.\n' for n in numbers
            )
            index = "".join(f"| {n} | [all](decisions/all.md#decision-{n}) | N |\n" for n in numbers)
            return {"docs/decisions/all.md": topic}, index

        topics, index = record(used)
        mentions = {"docs/ADR.md": "DECISIONS 524\nDECISIONS 522, 523 and 527\nDECISIONS 520–530"}
        self.assertEqual(cd.check(topics, index, mentions), [])

        topics, index = record([n for n in used if n != 522])
        self.assertIn(
            "docs/DECISIONS.md: 522 has neither an entry nor an index row; it was lost",
            cd.check(topics, index, {}),
        )

        topics, index = record(used + [524])
        errors = " ".join(cd.check(topics, index, {}))
        self.assertIn("524 is a new number in the closed sequence", errors)
        self.assertIn("row 524 takes a number the closed sequence never used", errors)

        # A small fixture's sequence does not reach them, so they are not holes in it.
        self.assertEqual(
            run(cited={"docs/ADR.md": "DECISIONS 523"}),
            ["docs/ADR.md:1: DECISIONS 523 names no entry"],
        )

    # #848: every number a grouped citation names is checked.

    def test_every_member_of_a_grouped_legacy_citation_is_checked(self):
        for line in [
            "DECISIONS 1, 9",
            "DECISIONS 1 and 9",
            "DECISIONS 1, 2, and 9",
            "DECISIONS 1, 2 and 9",
            "DECISIONS 2–3, 9",
            "DECISIONS 9, 1",
        ]:
            self.assertEqual(
                run(cited={"src/a.rs": line}), ["src/a.rs:1: DECISIONS 9 names no entry"], line
            )
        self.assertEqual(run(cited={"src/a.rs": "DECISIONS 1, 2 and 4"}), [])
        self.assertEqual(
            run(cited={"src/a.rs": "DECISIONS 1, 9 and 4; DECISIONS 2, 7"}),
            ["src/a.rs:1: DECISIONS 9 names no entry", "src/a.rs:1: DECISIONS 7 names no entry"],
        )

    def test_range_interiors_and_endpoints_are_checked(self):
        self.assertEqual(run(cited={"src/a.rs": "DECISIONS 1–4 and 2-3, 4–4"}), [])
        for line in ["DECISIONS 1–9", "DECISIONS 1-9", "DECISIONS 1 - 9"]:
            self.assertEqual(
                run(cited={"src/a.rs": line}), ["src/a.rs:1: DECISIONS 9 names no entry"], line
            )
        # A middle number gone: the range's endpoints are there, its interior is not.
        identity = IDENTITY.split('<a id="decision-3">')[0]
        index = INDEX.replace("| 3 | [identity](decisions/identity.md#decision-3) | Three |\n", "")
        errors = run({"docs/decisions/identity.md": identity}, {"src/a.rs": "DECISIONS 2–4"}, index)
        self.assertIn("src/a.rs:1: DECISIONS 3 names no entry", errors)
        self.assertEqual(
            run(cited={"src/a.rs": "DECISIONS 4–2"}),
            ["src/a.rs:1: DECISIONS 4–2 is a range that runs backwards"],
        )
        self.assertEqual(
            run(cited={"src/a.rs": "DECISIONS 0"}), ["src/a.rs:1: DECISIONS 0 names no entry"]
        )
        self.assertEqual(
            run(cited={"src/a.rs": "DECISIONS 2–900000000"}),
            ["src/a.rs:1: DECISIONS 900000000 names no entry"],
        )

    def test_group_boundaries_preserve_prose_and_exclusions(self):
        for line in [
            "DECISIONS 2, and the difference is 9 times smaller",
            "DECISIONS 1, DEC-737.1 and 9",
            "DECISIONS 4 - the reason, 9",
            "DECISIONS 4–, issue 9",
            "(DECISIONS 1, 2), 9",
            "DECISIONS 1 and the 9 others",
        ]:
            self.assertEqual(
                run({"docs/decisions/identity.md": IDENTITY + new_entry(737, 1)}, {"src/a.rs": line}),
                [],
                line,
            )
        self.assertEqual(
            run(cited={"src/a.rs": "DECISIONS 1, DEC-800.1"}),
            ["src/a.rs:1: DEC-800.1 names no entry"],
        )
        self.assertEqual(run(cited={"scripts/check-decisions.py": "DECISIONS 1, 9"}), [])

    def test_the_index_is_not_read_as_a_citation(self):
        self.assertEqual(run(cited={"docs/DECISIONS.md": "e.g. `DEC-737.1`"}), [])


if __name__ == "__main__":
    unittest.main()
