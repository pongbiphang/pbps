#!/usr/bin/env python3
"""A stale or unreadable fixture must never qualify a matrix cell."""

import copy
import importlib.util
from pathlib import Path
import unittest


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


if __name__ == "__main__":
    unittest.main()
