#!/usr/bin/env python3
"""An unreadable or ambiguous inventory must not look like zero work."""
from contextlib import redirect_stderr, redirect_stdout
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import sys
from tempfile import TemporaryDirectory
import unittest
from unittest.mock import call, patch

spec=importlib.util.spec_from_file_location("measure",Path(__file__).with_name("order-closure-measures.py"))
m=importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


class Inventory(unittest.TestCase):
    def test_unique_symbol_records_its_source_line(self):
        item=m.site("a.rs","f","fn f(","rule")
        with patch.object(m,"git",return_value="// header\nfn f() {}\n"):
            result=m.snapshot("a"*40,[item])
        self.assertEqual(result["sites"][0]["line"],2)
        self.assertEqual(result["distinct_sites"],1)

    def test_missing_and_duplicate_anchors_refuse_the_inventory(self):
        item=m.site("a.rs","f","fn f(","rule")
        for source in ["fn g() {}", "fn f() {}\nfn f() {}"]:
            with patch.object(m,"git",return_value=source):
                with self.assertRaises(ValueError):
                    m.snapshot("a"*40,[item])

    def test_unreadable_git_source_propagates_instead_of_counting_empty(self):
        item=m.site("a.rs","f","fn f(","rule")
        with patch.object(m,"git",side_effect=subprocess.CalledProcessError(128,["git"])):
            with self.assertRaises(subprocess.CalledProcessError):
                m.snapshot("a"*40,[item])

    def test_every_snapshot_uses_refs_pinned_once_before_any_section(self):
        before_sha="a"*40
        after_sha="b"*40
        resolutions=[call("rev-parse","--verify","HEAD~1^{commit}"),
                     call("rev-parse","--verify","HEAD^{commit}")]
        with TemporaryDirectory() as directory:
            output=Path(directory)/"report.json"
            argv=["measure","--before","HEAD~1","--after","HEAD","--output",str(output)]
            with patch.object(sys,"argv",argv), \
                 patch.object(m,"git",side_effect=[before_sha+"\n",after_sha+"\n"]) as git, \
                 redirect_stdout(io.StringIO()):
                def pinned_snapshot(sha, sites):
                    self.assertEqual(git.call_args_list,resolutions)
                    self.assertIn(sha,(before_sha,after_sha))
                    return dict(sha=sha,sites=[],distinct_sites=0,files=0)

                with patch.object(m,"snapshot",side_effect=pinned_snapshot) as snapshot:
                    m.main()
                self.assertEqual(git.call_args_list,resolutions)
            expected=[call(before_sha,m.BEFORE),call(after_sha,m.AFTER),
                      call(before_sha,m.COMMON),call(after_sha,m.COMMON)]
            for case in m.CASES.values():
                expected.extend([call(before_sha,case["before"]),call(after_sha,case["after"])])
            self.assertEqual(snapshot.call_args_list,expected)
            report=json.loads(output.read_text())
            self.assertEqual(report["before_sha"],before_sha)
            self.assertEqual(report["after_sha"],after_sha)

    def test_unresolvable_ref_exits_two_before_building_any_section(self):
        error=subprocess.CalledProcessError(128,["git","rev-parse"])
        for failed_ref,results in [("before",[error]),("after",["a"*40+"\n",error])]:
            with self.subTest(failed_ref=failed_ref), TemporaryDirectory() as directory:
                output=Path(directory)/"report.json"
                argv=["measure","--before","HEAD~1","--after","HEAD","--output",str(output)]
                with patch.object(sys,"argv",argv), \
                     patch.object(m,"git",side_effect=results), \
                     patch.object(m,"snapshot",return_value=dict(sha="unused",sites=[],distinct_sites=0,files=0)) as snapshot, \
                     redirect_stdout(io.StringIO()), redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as raised:
                        m.main()
                self.assertEqual(raised.exception.code,2)
                snapshot.assert_not_called()
                self.assertFalse(output.exists())


if __name__ == "__main__":
    unittest.main()
