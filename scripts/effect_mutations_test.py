#!/usr/bin/env python3
"""Mutation kills need exact restored-source confirmations."""
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

    def test_a_failed_harness_run_still_requires_confirmation(self):
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

    def mutation_with_confirmation(self, confirmation_outputs, names=("order::tests::fixture",)):
        name,anchor,replacement=m.S7_MUTATIONS[0]
        command=["cargo","test","-p","pbps-diff","--lib"]
        good="test result: ok. 1 passed; 0 failed"
        bad="\n".join(f"test {name} ... FAILED" for name in names)
        bad+=f"\ntest result: FAILED. 0 passed; {max(1,len(names))} failed"
        outputs=[(0,good),(101,bad),*confirmation_outputs]
        with tempfile.TemporaryDirectory() as directory:
            effects=Path(directory)/"effects.rs"
            original=("// fixture\n"+anchor).encode()
            effects.write_bytes(original)
            out=Path(directory)/"audit"
            calls=[]
            def run(cmd, **kwargs):
                calls.append(cmd)
                self.assertEqual(effects.read_bytes()==original,len(calls)!=2)
                code,log=outputs.pop(0)
                return SimpleNamespace(returncode=code,stdout=log,stderr="")
            with patch.object(m,"EFFECTS",effects), patch.object(m,"S7_COMMANDS",[command]), \
                 patch.object(m.subprocess,"run",side_effect=run), \
                 patch.object(m.subprocess,"check_output",return_value="a"*40+"\n"), \
                 patch.dict(os.environ,{"PBPS_TEST_DB":"fixture","PBPS_TEST_PG_DB":"fixture"}):
                result=m.s7_main(["--output",str(out),"CreateTable"])
            self.assertEqual(effects.read_bytes(),original)
            return result,json.loads((out/"report.json").read_text())["mutations"][0],calls

    def test_a_failure_that_persists_on_restored_source_is_invalid(self):
        result,mutation,calls=self.mutation_with_confirmation([
            (101,"test order::tests::fixture ... FAILED\ntest result: FAILED. 0 passed; 1 failed")])
        self.assertEqual(result,2)
        self.assertEqual(mutation["status"],"invalid")
        confirmation=mutation["confirmation"]
        self.assertEqual(confirmation["status"],"invalid")
        self.assertEqual(confirmation["runs"][0]["returncode"],101)
        self.assertIn("FAILED",confirmation["runs"][0]["log"])
        self.assertEqual(len(calls),3)

    def test_a_failure_that_passes_on_restored_source_is_killed(self):
        result,mutation,calls=self.mutation_with_confirmation([(0,"test result: ok. 1 passed; 0 failed")])
        self.assertEqual(result,0)
        self.assertEqual(mutation["status"],"killed")
        self.assertIn("confirmation",mutation)
        confirmation=mutation["confirmation"]
        self.assertEqual(confirmation["status"],"passed")
        self.assertEqual(confirmation["runs"][0]["command"],
                         ["cargo","test","-p","pbps-diff","--lib","--","--exact","order::tests::fixture"])
        self.assertEqual(confirmation["runs"][0]["status"],"passed")
        self.assertIn("1 passed",confirmation["runs"][0]["log"])
        self.assertEqual(len(calls),3)

    def test_an_empty_failed_test_list_is_invalid(self):
        result,mutation,calls=self.mutation_with_confirmation([],names=())
        self.assertEqual(result,2)
        self.assertEqual(mutation["status"],"invalid")
        self.assertEqual(mutation["confirmation"]["reason"],"empty failed-test list")
        self.assertEqual(len(calls),2)

    def test_every_failed_test_must_pass_on_restored_source(self):
        result,mutation,calls=self.mutation_with_confirmation([
            (0,"test result: ok. 1 passed; 0 failed"),
            (101,"test fixture::second ... FAILED\ntest result: FAILED. 0 passed; 1 failed")],
            names=("fixture::first","fixture::second"))
        self.assertEqual(result,2)
        self.assertEqual(mutation["status"],"invalid")
        self.assertEqual([run["test"] for run in mutation["confirmation"]["runs"]],
                         ["fixture::first","fixture::second"])
        self.assertEqual(len(calls),4)

    def test_a_confirmation_selecting_no_tests_is_invalid(self):
        result,mutation,_=self.mutation_with_confirmation([(0,"test result: ok. 0 passed; 0 failed")])
        self.assertEqual(result,2)
        self.assertEqual(mutation["status"],"invalid")

    def test_live_confirmation_keeps_ignored_mode_and_replaces_the_suite_filter(self):
        command=["cargo","test","-p","pbps-pg","--test","live","--","--ignored","--nocapture","suite"]
        run={"command":command,"status":"failed","failed_tests":["commutation::suite"]}
        output=SimpleNamespace(returncode=0,stdout="test result: ok. 1 passed; 0 failed",stderr="")
        with patch.object(m.subprocess,"run",return_value=output) as subprocess_run:
            status,confirmation=m.mutation_result([run],{"PBPS_TEST_PG_DB":"fixture"})
        self.assertEqual(status,"killed")
        exact=["cargo","test","-p","pbps-pg","--test","live","--","--exact","commutation::suite","--ignored","--nocapture"]
        self.assertEqual(confirmation["runs"][0]["command"],exact)
        subprocess_run.assert_called_once_with(exact,capture_output=True,text=True,env={"PBPS_TEST_PG_DB":"fixture"})

    def test_a_persistent_legacy_failure_is_invalid(self):
        name,anchor,replacement=m.S7_MUTATIONS[0]
        good="test result: ok. 1 passed; 0 failed"
        bad="test order::tests::fixture ... FAILED\ntest result: FAILED. 0 passed; 1 failed"
        outputs=[SimpleNamespace(returncode=code,stdout=log,stderr="")
                 for code,log in [(0,good),(101,bad),(101,bad)]]
        with tempfile.TemporaryDirectory() as directory:
            effects=Path(directory)/"effects.rs"
            effects.write_text(anchor)
            def run(*args, **kwargs):
                if len(outputs)==1:
                    self.assertEqual(effects.read_text(),anchor)
                return outputs.pop(0)
            with patch.object(m,"EFFECTS",effects), patch.object(m.subprocess,"run",side_effect=run):
                self.assertEqual(m.legacy_main(["CreateTable"]),2)
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
