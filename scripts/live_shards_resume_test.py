#!/usr/bin/env python3
"""Regressions for resuming a stopped sharded gate by receipts (DEC-2553.1).

Every case runs the real driver, shard entry and cargo proxy from a copy of
`scripts/` inside a throwaway git repository, against a fake `cargo`, `docker`
and `free`. No fixture is started. Run: python3 scripts/live_shards_resume_test.py
"""

import datetime
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ("live-shards.py", "known_flakes.py", "ignored_test_inventory.py", "ci_release_scope.py")
GIT = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
CARGO = ["test", "-p", "demo", "--lib", "--"]
A_NAMES = [f"a{i:02}" for i in range(12)]
B_NAMES = [f"b{i:02}" for i in range(6)]

FAKE_CARGO = '''#!{python}
import json, os, sys
args = sys.argv[1:]
names = json.loads(os.environ["FAKE_A"]) if "--lib" in args else json.loads(os.environ["FAKE_B"])
if "--list" in args:
    print("\\n".join(n + ": test" for n in names))
    print(f"\\n{{len(names)}} tests, 0 benchmarks")
    sys.exit(0)
ran = args[args.index("--color=never") + 1:]
fail = set(json.loads(os.environ.get("FAKE_FAIL", "[]")))
die = os.environ.get("FAKE_DIE")
print(f"\\nrunning {{len(ran)}} tests", flush=True)
for name in ran:
    print(f"test {{name}} ... {{'FAILED' if name in fail else 'ok'}}", flush=True)
    if name == die:
        os._exit(137)
bad = sum(n in fail for n in ran)
print(f"\\ntest result: {{'FAILED' if bad else 'ok'}}. {{len(ran) - bad}} passed; {{bad}} failed; "
      "0 ignored; 0 measured; 0 filtered out; finished in 0.01s")
sys.exit(101 if bad else 0)
'''

RUNNER = '''#!/usr/bin/env bash
set -euo pipefail
if [[ -n ${PBPS_LIVE_SHARD:-} && ${PBPS_LIVE_ACTIVE:-} != 1 ]]; then
    exec python3 scripts/live-shards.py shard pg "$@"
fi
cargo test -p demo --lib -- --ignored "$@"
cargo test -p demo --test second -- --ignored "$@"
'''


def git(repo, *args):
    return subprocess.run([*GIT, *args], cwd=repo, text=True, capture_output=True, check=True).stdout.strip()


def load_shards(path):
    spec = importlib.util.spec_from_file_location("live_shards_copy", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class Harness(unittest.TestCase):
    """A repository whose scripts/ is a copy of this one's, plus stubs on PATH."""

    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory(prefix="pbps-resume-")
        base = Path(cls.directory.name)
        cls.repo = base / "repo"
        (cls.repo / "scripts").mkdir(parents=True)
        for name in SCRIPTS:
            shutil.copy(ROOT / "scripts" / name, cls.repo / "scripts" / name)
        (cls.repo / "scripts/live-tests-pg.sh").write_text(RUNNER)
        (cls.repo / "scripts/known-flakes.json").write_text('{"version": 1, "flakes": []}\n')
        (cls.repo / ".gitignore").write_text("target/\n__pycache__/\n")
        git(cls.repo, "init", "-q", "-b", "main")
        git(cls.repo, "add", "-A")
        git(cls.repo, "commit", "-q", "-m", "base")
        cls.bin = base / "bin"
        cls.bin.mkdir()
        (cls.bin / "cargo").write_text(FAKE_CARGO.format(python=sys.executable))
        (cls.bin / "docker").write_text("#!/bin/sh\nexit 0\n")
        (cls.bin / "free").write_text("#!/bin/sh\necho 'Mem: 65536 0 65536 0 0 65536'\n")
        for stub in cls.bin.iterdir():
            stub.chmod(0o755)
        # The runner stub names python3; it must be the interpreter CI pins.
        (cls.bin / "python3").symlink_to(sys.executable)
        cls.shards = load_shards(cls.repo / "scripts/live-shards.py")
        cls.runs = base / "runs"
        cls.runs.mkdir()
        cls.counter = 0

    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def drive(self, *options, fail=(), die=None, shards=2, output=None, check=None, env=None, a_names=A_NAMES):
        """Run the driver; returns (CompletedProcess, output directory)."""
        type(self).counter += 1
        output = output or self.runs / f"run{self.counter}"
        process_env = dict(os.environ, PATH=f"{self.bin}{os.pathsep}{os.environ['PATH']}",
                           FAKE_A=json.dumps(a_names), FAKE_B=json.dumps(B_NAMES), FAKE_FAIL=json.dumps(list(fail)))
        for key in [k for k in process_env if k.startswith(("PBPS_LIVE_", "PBPS_TEST_"))]:
            del process_env[key]
        process_env.update(env or {})
        env = process_env
        if die:
            env["FAKE_DIE"] = die
        result = subprocess.run(
            [sys.executable, "scripts/live-shards.py", "--suite", "pg", "--pg-shards", str(shards),
             "--prefix", "s2553", "--output", str(output), "--flakes-ref", "HEAD", *options],
            cwd=self.repo, env=env, text=True, capture_output=True, timeout=120)
        if check is not None:
            self.assertEqual(result.returncode, check, result.stdout + result.stderr)
        return result, output

    def summary(self, output):
        return json.loads((output / "summary.json").read_text())

    def stopped_run(self, **options):
        result, output = self.drive(die=options.pop("die", "a05"), **options)
        self.assertNotEqual(result.returncode, 0)
        return output

    def receipts(self, output):
        return {p: json.loads(p.read_text()) for p in sorted(output.glob("pg/*/*.json"))}

    def copy(self, output):
        target = self.runs / f"copy{self.counter}-{output.name}"
        type(self).counter += 1
        shutil.copytree(output, target)
        return target

    def set_register(self, entries):
        (self.repo / "scripts/known-flakes.json").write_text(json.dumps({"version": 1, "flakes": entries}))
        git(self.repo, "commit", "-q", "-am", "register")

    def tearDown(self):
        # Leave the repository at its recorded state for the next case.
        (self.repo / "scripts/known-flakes.json").write_text('{"version": 1, "flakes": []}\n')
        subprocess.run([*GIT, "commit", "-q", "-am", "reset"], cwd=self.repo, capture_output=True)
        subprocess.run([*GIT, "clean", "-fdq", "-e", "target"], cwd=self.repo, capture_output=True)
        subprocess.run([*GIT, "checkout", "-q", "--", "."], cwd=self.repo, capture_output=True)


class PlainRun(Harness):
    def test_a_plain_run_writes_the_identity_and_audits_exactly_as_before(self):
        result, output = self.drive(check=0)
        summary = self.summary(output)
        self.assertEqual(summary["pg"]["union"], "exact")
        self.assertEqual(summary["pg"]["passed"], len(A_NAMES) + len(B_NAMES))
        self.assertNotIn("carried", summary["pg"])
        tree = git(self.repo, "rev-parse", "HEAD^{tree}")
        manifest = json.loads((output / "manifest.json").read_text())
        self.assertEqual((manifest["tree"], manifest["suites"], manifest["resumed_from"]), (tree, ["pg"], None))
        self.assertRegex(manifest["inputs"], "^[0-9a-f]{64}$")
        self.assertEqual(summary["run"]["run_id"], manifest["run_id"])
        for row in self.receipts(output).values():
            self.assertEqual((row["tree"], row["inputs"], row["carried"]), (tree, manifest["inputs"], {}))

    def test_old_shaped_receipts_still_audit_and_no_temporary_file_is_left(self):
        _, output = self.drive(check=0)
        self.assertEqual(list(output.glob("pg/*/*.tmp")), [])
        for path, row in self.receipts(output).items():
            for field in ("tree", "inputs", "carried"):
                del row[field]
            path.write_text(json.dumps(row))
        result, _ = self.shards_audit(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout), {"failed": 0, "passed": 18, "union": "exact"})

    def shards_audit(self, output, count=2):
        result = subprocess.run([sys.executable, "scripts/live-shards.py", "audit", str(output / "pg"), str(count)],
                                cwd=self.repo, text=True, capture_output=True)
        return result, output

    def test_a_dirty_worktree_gives_the_run_no_identity(self):
        (self.repo / "scripts/stray.py").write_text("x = 1\n")
        _, output = self.drive(check=0)
        self.assertIsNone(json.loads((output / "manifest.json").read_text())["tree"])
        self.assertTrue(all(row["tree"] is None for row in self.receipts(output).values()))


class Resume(Harness):
    def test_resume_over_a_stopped_shard_runs_only_what_was_not_proven(self):
        stopped = self.stopped_run()
        (stopped / "summary.json").unlink()  # an interrupted driver never writes it
        proven = sum(r["status"] == "ok" for row in self.receipts(stopped).values() if row["valid"]
                     for r in row["reported"].values())
        result, output = self.drive("--resume", str(stopped), check=0)
        summary = self.summary(output)
        total = len(A_NAMES) + len(B_NAMES)
        self.assertEqual((summary["pg"]["union"], summary["pg"]["passed"], summary["pg"]["failed"]),
                         ("exact", total, 0))
        self.assertEqual(summary["pg"]["carried"], proven)
        self.assertLess(0, proven)
        ran = sum(len(row["reported"]) for row in self.receipts(output).values())
        self.assertEqual(ran + proven, total)
        resumed = summary["run"]["resumed_from"]
        self.assertEqual(resumed["run_id"], json.loads((stopped / "manifest.json").read_text())["run_id"])
        self.assertIsNone(resumed["summary_sha256"])
        audited, _ = PlainRun.shards_audit(self, output)
        self.assertEqual(json.loads(audited.stdout)["union"], "exact")

    def test_resume_may_use_another_shard_count(self):
        stopped = self.stopped_run(shards=3)
        _, output = self.drive("--resume", str(stopped), shards=2, check=0)
        self.assertEqual(self.summary(output)["pg"]["union"], "exact")

    def test_a_name_failed_in_run_one_and_ok_in_run_two_counts_once(self):
        self.set_register([{"test": "a03", "cargo": CARGO, "issue": 1, "expires": self.soon()}])
        first, failed = self.drive(fail=["a03"])
        self.assertNotEqual(first.returncode, 0)
        _, output = self.drive("--resume", str(failed), check=0)
        summary = self.summary(output)
        self.assertEqual(summary["pg"]["passed"], len(A_NAMES) + len(B_NAMES))
        self.assertEqual(summary["run"]["resumed_from"]["flakes"],
                         [{"test": "a03", "cargo": CARGO, "issue": 1}])
        reported = [n for row in self.receipts(output).values() for n in row["reported"]]
        self.assertEqual(reported.count("a03"), 1)
        self.assertNotIn("a03", [n for row in self.receipts(output).values() for n in row["carried"]])

    def soon(self, days=10):
        return (datetime.datetime.now(datetime.timezone.utc).date() + datetime.timedelta(days=days)).isoformat()

    def test_a_resumed_run_can_be_resumed_again_and_keeps_provenance(self):
        stopped = self.stopped_run()
        again, second = self.drive("--resume", str(stopped), die="b03")
        self.assertNotEqual(again.returncode, 0)
        _, third = self.drive("--resume", str(second), check=0)
        summary = self.summary(third)
        self.assertEqual((summary["pg"]["union"], summary["pg"]["passed"]), ("exact", 18))
        runs = {r["run"] for row in self.receipts(third).values() for r in row["carried"].values()}
        self.assertEqual(len(runs), 2)

    def test_a_flake_failing_again_fails_the_run_and_a_third_failure_refuses(self):
        self.set_register([{"test": "a03", "cargo": CARGO, "issue": 1, "expires": self.soon()}])
        _, one = self.drive(fail=["a03"])
        two_result, two = self.drive("--resume", str(one), fail=["a03"])
        self.assertNotEqual(two_result.returncode, 0)
        self.assertEqual(self.summary(two)["pg"]["failed"], 1)
        three, third = self.drive("--resume", str(two), fail=["a03"])
        self.assertNotEqual(three.returncode, 0)
        refused, _ = self.drive("--resume", str(third))
        self.assertNotEqual(refused.returncode, 0)
        self.assertIn("more than 2 times", refused.stderr)

    def test_refused_resumes(self):
        stopped = self.stopped_run()
        # Each case mutates a copy and states the fragment the refusal must name.
        def edit_manifest(**changes):
            def mutate(copy):
                path = copy / "manifest.json"
                manifest = json.loads(path.read_text())
                manifest.update(changes)
                path.write_text(json.dumps(manifest))
            return mutate

        def edit_receipt(**changes):
            def mutate(copy):
                path = sorted(copy.glob("pg/*/*.json"))[0]
                row = json.loads(path.read_text())
                row.update(changes)
                path.write_text(json.dumps(row))
            return mutate

        cases = {
            "another head tree": (edit_manifest(tree="0" * 40), "previous tree"),
            "another inputs digest": (edit_manifest(inputs="1" * 64, inputs_env=["PBPS_TEST_DEV_IMAGE"]),
                                      "PBPS_TEST_DEV_IMAGE"),
            "no tree identity": (edit_manifest(tree=None), "no tree identity"),
            "unknown version": (edit_manifest(version=2), "unsupported previous manifest"),
            "other suites": (edit_manifest(suites=["mssql", "pg"]), "covered"),
            "no manifest": (lambda copy: (copy / "manifest.json").unlink(), "unreadable manifest"),
            "malformed manifest": (lambda copy: (copy / "manifest.json").write_text("{"), "unreadable manifest"),
            "no suite receipts": (lambda copy: shutil.rmtree(copy / "pg"), "no pg receipts"),
            "truncated receipt": (lambda copy: sorted(copy.glob("pg/*/*.json"))[0].write_text('{"args"'),
                                  "unreadable receipt"),
            "receipt without identity": (lambda copy: edit_receipt(tree=None)(copy), "another head"),
            "receipt of other inputs": (edit_receipt(inputs="2" * 64), "another head or inputs"),
            "receipt of the old shape": (lambda copy: [
                p.write_text(json.dumps({k: v for k, v in json.loads(p.read_text()).items() if k != "tree"}))
                for p in copy.glob("pg/*/*.json")], "lacks the head identity"),
            "result of an unknown status": (edit_receipt(reported={"a00": {"status": "SKIPPED", "seconds": 1}}),
                                            "malformed result"),
        }
        for name, (mutate, fragment) in cases.items():
            with self.subTest(name):
                copy = self.copy(stopped)
                mutate(copy)
                result, output = self.drive("--resume", str(copy))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(fragment, result.stderr)
                self.assertFalse(output.exists(), "a refused resume creates no output")

    def test_a_different_head_is_refused_before_anything_starts(self):
        stopped = self.stopped_run()
        (self.repo / "scripts/extra.py").write_text("x = 1\n")
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", "another head")
        result, output = self.drive("--resume", str(stopped))
        self.assertNotEqual(result.returncode, 0)
        self.assertRegex(result.stderr, r"previous tree [0-9a-f]{12} != current [0-9a-f]{12}")
        self.assertFalse(output.exists())

    def test_a_changed_environment_input_is_refused(self):
        stopped = self.stopped_run()
        result, _ = self.drive("--resume", str(stopped),
                               env={"PBPS_TEST_DEV_IMAGE": "example/other@sha256:" + "0" * 64})
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("previous inputs", result.stderr)

    def test_a_dirty_worktree_is_refused(self):
        stopped = self.stopped_run()
        (self.repo / "scripts/stray.py").write_text("x = 1\n")
        result, _ = self.drive("--resume", str(stopped))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("this run has no tree identity", result.stderr)

    def test_the_new_output_must_be_a_different_directory(self):
        stopped = self.stopped_run()
        result, _ = self.drive("--resume", str(stopped), output=stopped)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fresh --output", result.stderr)

    def test_an_unreadable_register_ref_refuses(self):
        stopped = self.stopped_run()
        result, _ = self.drive("--resume", str(stopped), "--flakes-ref", "no-such-ref")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("git show failed", result.stderr)

    def test_a_list_that_changed_since_the_previous_run_is_refused(self):
        stopped = self.stopped_run()
        result, output = self.drive("--resume", str(stopped), a_names=A_NAMES + ["a99"])
        self.assertNotEqual(result.returncode, 0)
        logs = "".join(p.read_text() for p in output.glob("pg/*.log"))
        self.assertIn("test list changed", logs)


class Flakes(Harness):
    def failed_run(self, **kwargs):
        result, output = self.drive(fail=["a03"], **kwargs)
        self.assertNotEqual(result.returncode, 0)
        return output

    def test_a_failure_outside_the_register_is_never_waived(self):
        failed = self.failed_run()
        result, _ = self.drive("--resume", str(failed))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pg: a03", result.stderr)

    def test_an_expired_entry_waives_nothing(self):
        self.set_register([{"test": "a03", "cargo": CARGO, "issue": 1, "expires": "2020-01-01"}])
        result, _ = self.drive("--resume", str(self.failed_run()))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pg: a03", result.stderr)

    def test_an_entry_for_another_invocation_or_name_waives_nothing(self):
        for entry in ({"test": "a03", "cargo": ["test", "-p", "demo", "--test", "second", "--"]},
                      {"test": "a04", "cargo": CARGO}):
            with self.subTest(entry=entry):
                self.set_register([dict(entry, issue=1, expires=Resume.soon(self))])
                result, _ = self.drive("--resume", str(self.failed_run()))
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("pg: a03", result.stderr)

    def test_the_head_cannot_waive_its_own_failure(self):
        self.set_register([{"test": "a03", "cargo": CARGO, "issue": 1, "expires": Resume.soon(self)}])
        failed = self.failed_run()
        # The entry exists on HEAD only; the register is read from the named ref.
        git(self.repo, "branch", "-f", "master-like", "HEAD~1")
        result, _ = self.drive("--resume", str(failed), "--flakes-ref", "master-like")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pg: a03", result.stderr)

    def test_a_failure_in_a_row_that_never_finished_still_counts(self):
        stopped = self.stopped_run(die="a05")
        for path in stopped.glob("pg/*/*.json"):
            row = json.loads(path.read_text())
            if not row["valid"]:
                name = next(iter(row["reported"]))
                row["reported"][name]["status"] = "FAILED"
                path.write_text(json.dumps(row))
        result, _ = self.drive("--resume", str(stopped))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("FAILED outside", result.stderr)


class Audit(unittest.TestCase):
    """The union audit over combined receipts, without running anything."""

    shards = load_shards(ROOT / "scripts/live-shards.py")

    def write(self, root, rows):
        for index, row in enumerate(rows):
            (root / str(index)).mkdir()
            full = {"args": CARGO + ["--ignored"], "identity": "bin", "pinned": False, "valid": True, "seconds": 1,
                    "tree": "t" * 40, "inputs": "i" * 64, "carried": {}}
            full.update(row)
            full["reported"] = {n: {"status": "ok", "seconds": 1} for n in full["assigned"]}
            full["groups"] = row["groups"]
            (root / str(index) / "call.json").write_text(json.dumps(full))

    def carried(self, *names, status="ok"):
        return {n: {"status": status, "seconds": 1, "run": "earlier"} for n in names}

    def audit(self, rows, count=2):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            self.write(root, rows)
            return self.shards.audit(root, count)[0]

    def test_ran_plus_carried_must_equal_the_list(self):
        listed = ["a", "b", "c", "d"]
        rows = [dict(listed=listed, groups=[["a"], ["b"]], assigned=["a"], carried=self.carried("c", "d")),
                dict(listed=listed, groups=[["a"], ["b"]], assigned=["b"], carried=self.carried("c", "d"))]
        self.assertEqual(self.audit(rows), {"passed": 4, "failed": 0, "union": "exact", "carried": 2})

    def test_a_name_missing_in_both_runs_is_refused(self):
        listed = ["a", "b", "c", "d"]
        rows = [dict(listed=listed, groups=[["a"], ["b"]], assigned=["a"], carried=self.carried("c")),
                dict(listed=listed, groups=[["a"], ["b"]], assigned=["b"], carried=self.carried("c"))]
        with self.assertRaisesRegex(ValueError, "union differs"):
            self.audit(rows)

    def test_a_carried_name_that_also_ran_is_refused(self):
        listed = ["a", "b", "c"]
        rows = [dict(listed=listed, groups=[["a", "c"], ["b"]], assigned=["a", "c"], carried=self.carried("c")),
                dict(listed=listed, groups=[["a", "c"], ["b"]], assigned=["b"], carried=self.carried("c"))]
        with self.assertRaisesRegex(ValueError, "ran again"):
            self.audit(rows)

    def test_a_carried_name_that_is_not_ok_is_refused(self):
        listed = ["a", "b"]
        rows = [dict(listed=listed, groups=[["a"], []], assigned=["a"], carried=self.carried("b", status="FAILED")),
                dict(listed=listed, groups=[["a"], []], assigned=[], carried=self.carried("b", status="FAILED"))]
        with self.assertRaisesRegex(ValueError, "not ok"):
            self.audit(rows)

    def test_shards_that_disagree_about_the_carried_names_are_refused(self):
        listed = ["a", "b", "c"]
        rows = [dict(listed=listed, groups=[["a"], ["b"]], assigned=["a"], carried=self.carried("c")),
                dict(listed=listed, groups=[["a"], ["b"]], assigned=["b"], carried={})]
        with self.assertRaisesRegex(ValueError, "disagree about the carried"):
            self.audit(rows)

    def test_an_idle_shard_is_allowed_only_for_a_resumed_run(self):
        listed = ["a", "b"]
        resumed = [dict(listed=listed, groups=[["a"], []], assigned=["a"], carried=self.carried("b")),
                   dict(listed=listed, groups=[["a"], []], assigned=[], carried=self.carried("b"))]
        self.assertEqual(self.audit(resumed)["union"], "exact")
        plain = [dict(listed=listed, groups=[["a", "b"], []], assigned=["a", "b"]),
                 dict(listed=listed, groups=[["a", "b"], []], assigned=[])]
        with self.assertRaisesRegex(ValueError, "empty for the whole suite"):
            self.audit(plain)


class Identity(unittest.TestCase):
    shards = Audit.shards

    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pbps-identity-")
        self.addCleanup(self.directory.cleanup)
        self.repo = Path(self.directory.name)
        (self.repo / ".gitignore").write_text("target/\n__pycache__/\n")
        (self.repo / "a.txt").write_text("a\n")
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", "base")

    def test_a_clean_worktree_reports_its_tree_and_a_target_symlink_is_ignored(self):
        self.assertEqual(self.shards.tree_identity(self.repo), git(self.repo, "rev-parse", "HEAD^{tree}"))
        (self.repo / "target").symlink_to(self.directory.name)
        self.assertIsNotNone(self.shards.tree_identity(self.repo))

    def test_any_change_gives_no_identity(self):
        for change in (lambda: (self.repo / "a.txt").write_text("b\n"),
                       lambda: (self.repo / "new.rs").write_text("fn x() {}\n"),
                       lambda: (self.repo / "a.txt").unlink()):
            with self.subTest():
                change()
                self.assertIsNone(self.shards.tree_identity(self.repo))
                git(self.repo, "checkout", "-q", "--", ".")
                subprocess.run([*GIT, "clean", "-fdq"], cwd=self.repo, check=True)

    def test_a_directory_that_is_not_a_repository_gives_no_identity(self):
        with tempfile.TemporaryDirectory() as other:
            self.assertIsNone(self.shards.tree_identity(other))

    def digest(self, **env):
        base = {"PATH": os.environ["PATH"], "HOME": os.environ.get("HOME", "")}
        return self.shards.inputs_digest(dict(base, **env))[0]

    def test_the_inputs_digest_follows_what_changes_a_verdict(self):
        reference = self.digest()
        self.assertEqual(self.digest(), reference)
        for name in ("PBPS_TEST_DEV_IMAGE", "PBPS_TEST_PG_SCRATCH_IMAGE", "RUSTFLAGS", "INSTA_UPDATE",
                     "CARGO_PROFILE_DEV_DEBUG", "PBPS_SOMETHING_NEW"):
            with self.subTest(name):
                self.assertNotEqual(self.digest(**{name: "x"}), reference)

    def test_the_inputs_digest_ignores_scheduling_and_fixture_identity(self):
        reference = self.digest()
        for name in ("PBPS_LIVE_PREFIX", "PBPS_LIVE_WEIGHTS", "PBPS_LIVE_SHARD", "PBPS_TEST_PG_NAME",
                     "PBPS_TEST_PG_PORT", "PBPS_TEST_MSSQL_SCRATCH_NAME", "CARGO_BUILD_JOBS", "TMPDIR"):
            with self.subTest(name):
                self.assertEqual(self.digest(**{name: "x"}), reference)

    def test_an_unreadable_toolchain_is_an_error_not_an_empty_digest(self):
        with self.assertRaises((ValueError, OSError)):
            self.shards.inputs_digest({"PATH": "/nonexistent"})


if __name__ == "__main__":
    unittest.main()
