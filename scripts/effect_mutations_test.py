#!/usr/bin/env python3
"""Pins for mutation-audit evidence: compile/empty failures are not catches."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

spec=importlib.util.spec_from_file_location("mutations", Path(__file__).with_name("effect-mutations.py"))
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class Evidence(unittest.TestCase):
    def test_original_sample_keeps_every_kind_and_unique_current_anchor(self):
        text=subprocess.check_output(["git","show","HEAD:crates/pbps-model/src/operation/effects.rs"],text=True)
        self.assertEqual(tuple(x[0].split(":")[0] for x in m.S7_MUTATIONS), m.ORIGINAL_KINDS)
        self.assertEqual(len(m.S7_MUTATIONS), 20)
        for name, anchor, replacement in m.S7_MUTATIONS:
            self.assertEqual(text.count(anchor), 1, name)
            self.assertNotEqual(anchor, replacement, name)

    def test_an_assertion_failure_is_a_catch(self):
        self.assertEqual(m.test_status(101, "test result: FAILED. 3 passed; 1 failed"), "failed")

    def test_compiler_failure_and_empty_success_are_invalid(self):
        for code, log in [(101,"error[E0308]: mismatch"), (0,"test result: ok. 0 passed; 0 failed"),
                          (101,"test result: ok. 3 passed; 0 failed")]:
            self.assertEqual(m.test_status(code,log), "invalid")

    def test_all_green_tests_are_not_a_catch(self):
        self.assertEqual(m.test_status(0,"test result: ok. 3 passed; 0 failed"), "passed")

    def test_a_build_failure_is_not_hidden_by_another_engines_failed_test(self):
        anchor=m.S7_MUTATIONS[0][1]
        good="commutation audit\ntest result: ok. 1 passed; 0 failed"
        bad="test result: FAILED. 0 passed; 1 failed"
        outputs=[SimpleNamespace(returncode=code,stdout=log,stderr="")
                 for code,log in [(0,good),(0,good),(101,bad),(101,"error[E0308]")]]
        with tempfile.TemporaryDirectory() as d:
            effects=Path(d)/"effects.rs"
            effects.write_text(anchor)
            with patch.object(m,"EFFECTS",effects), patch.object(m.subprocess,"run",side_effect=outputs):
                self.assertEqual(m.legacy_main(["--audit","CreateTable"]),2)
            self.assertEqual(effects.read_text(),anchor)

    def test_interrupt_after_mutation_restores_the_exact_source_copy(self):
        name,anchor,replacement=m.S7_MUTATIONS[0]
        with tempfile.TemporaryDirectory() as d:
            effects=Path(d)/"effects.rs"
            original=("// fixture\n"+anchor).encode()
            effects.write_bytes(original)
            out=Path(d)/"audit"
            def run(directory, commands, env):
                if directory.name == "baseline":
                    return [{"status":"passed"}]
                self.assertNotEqual(effects.read_bytes(),original)
                raise KeyboardInterrupt
            with patch.object(m,"EFFECTS",effects), patch.object(m,"run_case",side_effect=run), \
                 patch.dict(os.environ,{"PBPS_TEST_DB":"fixture","PBPS_TEST_PG_DB":"fixture"}):
                with self.assertRaises(KeyboardInterrupt):
                    m.s7_main(["--output",str(out),"CreateTable"])
            self.assertEqual(effects.read_bytes(),original)
            self.assertEqual((out/"original-effects.rs").read_bytes(),original)
            report=json.loads((out/"report.json").read_text())
            self.assertEqual(report["source_sha256"],report["restored_sha256"])


if __name__ == "__main__":
    unittest.main()
