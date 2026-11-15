#!/usr/bin/env python3
"""A stale or unreadable fixture must never qualify a matrix cell."""

import contextlib
import copy
import importlib.util
import io
from pathlib import Path
import subprocess
import sys
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location("fixture", Path(__file__).with_name("check-live-fixture.py"))
fixture = importlib.util.module_from_spec(spec)
spec.loader.exec_module(fixture)


class Admission(unittest.TestCase):
    def setUp(self):
        self.row = fixture.MATRIX["mssql2025"]
        self.image = {"Id": "sha256:actual-config", "Os": "linux", "Architecture": "amd64"}
        self.container = {"State": {"Running": True}, "Image": self.image["Id"],
                          "NetworkSettings": {"Ports": {"1433/tcp": [
                              {"HostIp": "127.0.0.1", "HostPort": "14330"}]}}}

    def test_pinned_image_and_published_endpoint_are_both_required(self):
        fixture.validate_container(self.row, self.container, self.image, 14330)
        changes = [
            ("Image", "sha256:older-image"),
            ("State", {"Running": False}),
            ("NetworkSettings", {"Ports": {}}),
            ("NetworkSettings", {"Ports": {"1433/tcp": None}}),
            ("NetworkSettings", {"Ports": {"1433/tcp": [
                {"HostIp": "127.0.0.1", "HostPort": "14331"}]}}),
            ("NetworkSettings", {"Ports": {"1433/tcp": [
                {"HostIp": "192.0.2.1", "HostPort": "14330"}]}}),
        ]
        for key, value in changes:
            with self.subTest(key=key, value=value), self.assertRaises(ValueError):
                fixture.validate_container(self.row, {**self.container, key: value}, self.image, 14330)
        for key, value in [("Architecture", "arm64"), ("Os", "windows")]:
            with self.subTest(key=key), self.assertRaises(ValueError):
                fixture.validate_container(self.row, self.container, {**self.image, key: value}, 14330)

    def test_unreadable_inspection_is_not_an_empty_healthy_fixture(self):
        broken = copy.deepcopy(self.container)
        del broken["State"]
        with self.assertRaises(KeyError):
            fixture.validate_container(self.row, broken, self.image, 14330)

    def test_exact_build_edition_and_collation_are_not_capability_floors(self):
        for row in fixture.MATRIX.values():
            fields = [row["version"]]
            if row["engine"] == "mssql":
                fields += [row["edition"], str(row["engine_edition"]), row["collation"]]
            fixture.validate_server(row, fields)
            for index in range(len(fields)):
                changed = fields.copy()
                changed[index] += "-other"
                with self.subTest(row=row["version"], field=index), self.assertRaises(ValueError):
                    fixture.validate_server(row, changed)
            with self.assertRaises(ValueError):
                fixture.validate_server(row, [])


class Survival(unittest.TestCase):
    """A fixture that answered its probe must still be up a few seconds later (#2875)."""

    DUMP = "Dump already generated: /var/opt/mssql/log/core.sqlservr.11_15_2026_0_44_48.8\n"

    def survive(self, answers, log=("", ""), log_status=0, seconds=3):
        """Run `survive` against a daemon that answers `answers` in turn (the
        last one from then on) and a log of `log`, on a clock the polls advance.
        Returns the verdict, what it asked and the log it kept."""
        now = [0.0]
        asked = []

        def docker(*args):
            self.assertEqual(args[:2], ("container", "inspect"))
            answer = answers[min(len(asked), len(answers) - 1)]
            asked.append(answer)
            if isinstance(answer, Exception):
                raise answer
            return answer + "\n"

        def sleep(seconds_):
            now[0] += seconds_

        logs = subprocess.CompletedProcess(["docker", "logs"], log_status, *log)
        with mock.patch.object(fixture, "docker", docker), \
                mock.patch.object(fixture.subprocess, "run", return_value=logs):
            verdict, kept = fixture.survive("fixture", seconds, clock=lambda: now[0], sleep=sleep)
        return verdict, len(asked), kept

    def test_a_fixture_still_running_when_the_window_ends_survived(self):
        self.assertEqual(self.survive(["true"]), ("survived", 4, ""))

    def test_an_exit_with_a_core_dump_in_the_log_is_a_crash_wherever_the_log_prints_it(self):
        for log in ((self.DUMP, ""), ("", self.DUMP)):
            with self.subTest(log=log):
                self.assertEqual(self.survive(["true", "true", "false"], log), ("crash", 3, self.DUMP))

    def test_a_crash_before_the_first_poll_is_still_a_crash(self):
        self.assertEqual(self.survive(["false"], (self.DUMP, "")), ("crash", 1, self.DUMP))

    def test_every_other_way_of_not_running_is_not_a_crash(self):
        cases = {
            "an ordinary exit": (["false"], ("Login failed for user 'sa'.\n", ""), 0),
            "no log": (["false"], ("", ""), 0),
            "an unreadable log that names a dump": (["false"], (self.DUMP, ""), 1),
            "a container the runtime lost": ([RuntimeError("gone")], ("", ""), 0),
            "an unreadable first answer": (["true", RuntimeError("daemon")], ("", ""), 0),
        }
        for label, (answers, log, status) in cases.items():
            with self.subTest(label):
                self.assertEqual(self.survive(answers, log, status)[0], "other")

    def test_an_empty_window_asks_once(self):
        self.assertEqual(self.survive(["true"], seconds=0), ("survived", 1, ""))

    def run_main(self, verdict, *arguments):
        argv = ["check-live-fixture.py", "--container", "fixture", *arguments]
        with mock.patch.object(sys, "argv", argv), \
                contextlib.redirect_stderr(io.StringIO()), \
                mock.patch.object(fixture, "survive", return_value=(verdict, "the log")) as survive, \
                mock.patch.object(fixture, "report") as report:
            try:
                fixture.main()
                status = 0
            except SystemExit as stop:
                status = stop.code
        return status, survive.call_args, report

    def test_only_a_crash_exits_with_the_retry_status_and_prints_the_log(self):
        self.assertEqual(fixture.CRASHED, 75)
        status, call, report = self.run_main("crash", "--survive", "15")
        reported = report.called
        self.assertEqual((status, call.args, reported), (75, ("fixture", 15.0), True))
        self.assertEqual(report.call_args.args, ("fixture", "the log"))
        for verdict in ("survived", "other"):
            with self.subTest(verdict):
                status, _, report = self.run_main(verdict, "--survive", "15")
                self.assertEqual((status, report.called), (0, False))

    def test_the_survival_mode_needs_no_cell_and_the_admission_still_does(self):
        self.assertEqual(self.run_main("survived", "--survive", "1")[0], 0)
        self.assertEqual(self.run_main("survived")[0], 2)
        self.assertEqual(self.run_main("survived", "--survive", "-1")[0], 2)


if __name__ == "__main__":
    unittest.main()
