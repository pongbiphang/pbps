#!/usr/bin/env python3
"""Tests for scripts/known_flakes.py. Run: python3 scripts/known_flakes_test.py"""

import datetime
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import known_flakes as kf  # noqa: E402

TODAY = datetime.date(2026, 10, 31)
CARGO = ["test", "-p", "pbps-pg", "--test", "live", "--"]
TARGETS = {("pbps-pg", "test", "live"): {"all": {"live::flaky_one", "live::stable"}, "ignored": set(), "sources": set()}}


def entry(**changes):
    base = {"test": "live::flaky_one", "cargo": CARGO, "issue": 2513, "expires": "2026-11-15"}
    base.update(changes)
    return base


def register(*entries):
    return json.dumps({"version": 1, "flakes": list(entries)})


class Load(unittest.TestCase):
    def test_a_valid_entry_and_an_empty_register_load(self):
        self.assertEqual(kf.load(register(entry()))[0]["issue"], 2513)
        self.assertEqual(kf.load(register()), [])

    def test_schema_violations_are_refused(self):
        for name, text, fragment in (
                ("invalid json", "{", "not valid JSON"),
                ("not an object", "[]", "must be"),
                ("wrong version", json.dumps({"version": 2, "flakes": []}), "must be"),
                ("unknown top-level key", json.dumps({"version": 1, "flakes": [], "x": 1}), "must be"),
                ("flakes not a list", json.dumps({"version": 1, "flakes": {}}), "must be"),
                ("entry not an object", register("x"), "not an object"),
                ("no issue", register({k: v for k, v in entry().items() if k != "issue"}), "missing ['issue']"),
                ("unknown field", register(entry(note="x")), "unknown ['note']"),
                ("issue zero", register(entry(issue=0)), "cites no open issue"),
                ("issue negative", register(entry(issue=-3)), "cites no open issue"),
                ("issue a string", register(entry(issue="2513")), "cites no open issue"),
                ("issue a bool", register(entry(issue=True)), "cites no open issue"),
                ("empty test", register(entry(test="")), "no test name"),
                ("cargo not a list", register(entry(cargo="test -p x --")), "invocation prefix"),
                ("cargo without --", register(entry(cargo=["test", "-p", "x"])), "invocation prefix"),
                ("cargo not test", register(entry(cargo=["build", "--"])), "invocation prefix"),
                ("expires not iso", register(entry(expires="31/10/2026")), "YYYY-MM-DD"),
                ("expires not a date", register(entry(expires="2026-13-45")), "not a date"),
                ("expires a number", register(entry(expires=20261115)), "YYYY-MM-DD"),
                ("duplicate", register(entry(), entry(issue=9)), "registered twice")):
            with self.subTest(name):
                with self.assertRaisesRegex(kf.RegisterError, fragment.replace("[", r"\[").replace("]", r"\]")):
                    kf.load(text)


class Active(unittest.TestCase):
    def test_an_expired_entry_is_inactive_and_an_unexpired_one_is_keyed_by_invocation_and_name(self):
        entries = kf.load(register(entry(), entry(test="live::stable", expires="2026-10-30"),
                                   entry(test="live::on_the_day", expires="2026-10-31")))
        active = kf.active(entries, TODAY)
        self.assertEqual(sorted(name for _, name in active), ["live::flaky_one", "live::on_the_day"])
        self.assertIn((tuple(CARGO), "live::flaky_one"), active)
        self.assertNotIn((tuple(CARGO[:-1] + ["--ignored"]), "live::flaky_one"), active)


class Check(unittest.TestCase):
    def run_check(self, *entries, strict=False, today=TODAY):
        kf.check(kf.load(register(*entries)), today, TARGETS, strict)

    def test_a_valid_entry_passes_both_modes(self):
        self.run_check(entry())
        self.run_check(entry(), strict=True)

    def test_a_vanished_test_name_is_refused(self):
        with self.assertRaisesRegex(kf.RegisterError, "no longer exists"):
            self.run_check(entry(test="live::renamed_away"))

    def test_a_cargo_target_that_does_not_exist_is_refused(self):
        for cargo in (["test", "-p", "pbps-pg", "--test", "gone", "--"], ["test", "-p", "nope", "--lib", "--"],
                      ["test", "--", ], ["test", "-p", "pbps-pg", "--bogus", "--"]):
            with self.subTest(cargo=cargo):
                with self.assertRaises(kf.RegisterError):
                    self.run_check(entry(cargo=cargo))

    def test_an_expired_entry_passes_the_ordinary_check_and_fails_the_strict_one(self):
        expired = entry(expires="2026-10-01")
        self.run_check(expired)
        with self.assertRaisesRegex(kf.RegisterError, "expired 2026-10-01"):
            self.run_check(expired, strict=True)

    def test_an_entry_more_than_thirty_days_out_fails_both_modes(self):
        far = entry(expires="2026-12-01")
        for strict in (False, True):
            with self.subTest(strict=strict):
                with self.assertRaisesRegex(kf.RegisterError, "more than 30 days out"):
                    self.run_check(far, strict=strict)
        self.run_check(entry(expires="2026-11-30"))


class Strict(unittest.TestCase):
    def changed(self, *paths):
        return lambda base: list(paths)

    def test_the_nightly_run_and_the_flag_are_strict(self):
        self.assertTrue(kf.strict_expiry("schedule", "", False, self.changed())[0])
        self.assertTrue(kf.strict_expiry("", "", True, self.changed())[0])

    def test_a_change_that_touches_the_register_is_strict(self):
        self.assertTrue(kf.strict_expiry("pull_request", "a" * 40, False, self.changed(kf.REGISTER, "x"))[0])
        self.assertTrue(kf.strict_expiry("merge_group", "a" * 40, False, self.changed(kf.REGISTER))[0])

    def test_a_change_that_leaves_the_register_alone_is_not_strict(self):
        self.assertFalse(kf.strict_expiry("pull_request", "a" * 40, False, self.changed("docs/SPEC.md"))[0])
        self.assertFalse(kf.strict_expiry("pull_request", "a" * 40, False,
                                          self.changed("scripts/known-flakes.json.bak", "x/scripts/known-flakes.json"))[0])

    def test_no_scope_is_not_strict_but_an_unreadable_diff_is(self):
        self.assertFalse(kf.strict_expiry("", "", False, self.changed())[0])
        self.assertFalse(kf.strict_expiry("workflow_dispatch", "", False, self.changed())[0])

        def unreadable(base):
            raise OSError("no such commit")
        strict, reason = kf.strict_expiry("pull_request", "a" * 40, False, unreadable)
        self.assertTrue(strict)
        self.assertIn("could not be read", reason)


class ChangedSince(unittest.TestCase):
    def test_a_base_the_clone_has_is_diffed_without_fetching_or_making_the_clone_shallow(self):
        git = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
        with tempfile.TemporaryDirectory() as directory:
            def run(*args):
                return subprocess.run([*git, *args], cwd=directory, text=True, capture_output=True, check=True).stdout.strip()
            run("init", "-q", "-b", "main")
            Path(directory, "a").write_text("1")
            run("add", "-A")
            run("commit", "-q", "-m", "one")
            base = run("rev-parse", "HEAD")
            Path(directory, kf.REGISTER).parent.mkdir(parents=True)
            Path(directory, kf.REGISTER).write_text("{}")
            run("add", "-A")
            run("commit", "-q", "-m", "two")
            previous = os.getcwd()
            os.chdir(directory)
            try:
                self.assertEqual(kf.changed_since(base), [kf.REGISTER])
                self.assertEqual(run("rev-parse", "--is-shallow-repository"), "false")
                with self.assertRaises(subprocess.CalledProcessError):  # no remote to fetch a missing base from
                    kf.changed_since("0" * 40)
            finally:
                os.chdir(previous)


if __name__ == "__main__":
    unittest.main()
