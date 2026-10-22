#!/usr/bin/env python3
"""An unreadable or ambiguous inventory must not look like zero work."""
import importlib.util
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location("measure",Path(__file__).with_name("order-closure-measures.py"))
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class Inventory(unittest.TestCase):
    def test_unique_symbol_records_its_source_line(self):
        item=m.site("a.rs","f","fn f(","rule")
        with patch.object(m,"git",side_effect=["sha\n","// header\nfn f() {}\n"]):
            result=m.snapshot("ref",[item])
        self.assertEqual(result["sites"][0]["line"],2)
        self.assertEqual(result["distinct_sites"],1)

    def test_missing_and_duplicate_anchors_refuse_the_inventory(self):
        item=m.site("a.rs","f","fn f(","rule")
        for source in ["fn g() {}", "fn f() {}\nfn f() {}"]:
            with patch.object(m,"git",side_effect=["sha\n",source]):
                with self.assertRaises(ValueError):
                    m.snapshot("ref",[item])

    def test_unreadable_git_source_propagates_instead_of_counting_empty(self):
        with patch.object(m,"git",side_effect=subprocess.CalledProcessError(128,["git"])):
            with self.assertRaises(subprocess.CalledProcessError):
                m.snapshot("missing",[])


if __name__ == "__main__":
    unittest.main()
