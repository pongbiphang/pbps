#!/usr/bin/env python3
"""Regressions for resuming a stopped sharded gate by receipts (DEC-2553.1).

Every case runs the real driver, shard entry and cargo proxy from a copy of
`scripts/` inside a throwaway git repository, against a fake `cargo`, `docker`
and `free`. No fixture is started. Run: python3 scripts/live_shards_resume_test.py
"""

import contextlib
import datetime
import hashlib
import importlib.util
import io
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
SCRIPTS = ("live-shards.py", "live-transport.py", "known_flakes.py", "ignored_test_inventory.py",
           "ci_release_scope.py")
GIT = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
CARGO = ["test", "-p", "demo", "--lib", "--"]
A_NAMES = [f"a{i:02}" for i in range(12)]
B_NAMES = [f"b{i:02}" for i in range(6)]

FAKE_CARGO = '''#!{python}
import json, os, sys
args = sys.argv[1:]
names = json.loads(os.environ["FAKE_A"]) if "--lib" in args else json.loads(os.environ["FAKE_B"])
if "--test" in args and args[args.index("--test") + 1] == "tls":
    names = [args[args.index("--exact") + 1]]
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


# The real runners run the TLS helper after the sharded suites, on shard zero only.
RUNNER_TLS = RUNNER + '''if [[ -z ${PBPS_LIVE_SHARD:-} ]]; then
    python3 scripts/tls_case.py
elif [[ ${PBPS_LIVE_SHARD%%/*} == 0 ]]; then
    PBPS_LIVE_PINNED=1 python3 scripts/tls_case.py
fi
'''

# Drives the real `run_case` the way `live-transport.py main` does: one case, then one case three times.
TLS_CASE = '''import importlib.util, os
from pathlib import Path
spec = importlib.util.spec_from_file_location("transport", Path(__file__).with_name("live-transport.py"))
transport = importlib.util.module_from_spec(spec)
spec.loader.exec_module(transport)
cargo = ["cargo", "test", "-p", "demo", "--test", "tls", "--"]
transport.run_case(cargo, "tls_verified", dict(os.environ))
for _ in range(3):
    transport.run_case(cargo, "tls_untrusted", dict(os.environ))
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


class Chain(Harness):
    """A resume's evidence is the whole chain behind it, not only the last link."""

    def proved(self, output):
        """Names this run's receipts prove or carry, by their own account."""
        names = set()
        for row in self.receipts(output).values():
            names |= set(row["carried"])
            if row["valid"]:
                names |= {n for n, r in row["reported"].items() if r["status"] == "ok"}
        return names

    def ran(self, output):
        return {n for row in self.receipts(output).values() for n in row["reported"]}

    def lose_a_receipts(self, output):
        """Model a run killed before the `--lib` invocation wrote any receipt."""
        lost = [p for p, row in self.receipts(output).items() if "--lib" in row["args"]]
        self.assertTrue(lost)
        for path in lost:
            path.unlink()
        (output / "summary.json").unlink(missing_ok=True)

    def test_a_resume_interrupted_before_its_receipts_still_inherits_the_first_runs_names(self):
        first = self.stopped_run()
        proven = self.proved(first)
        self.assertTrue(proven & set(A_NAMES))
        _, second = self.drive("--resume", str(first), check=0)
        self.lose_a_receipts(second)
        _, third = self.drive("--resume", str(second), check=0)
        self.assertEqual(self.ran(third) & proven, set(), "names proven by the first run are rerun")
        summary = self.summary(third)
        self.assertEqual((summary["pg"]["union"], summary["pg"]["passed"]), ("exact", len(A_NAMES) + len(B_NAMES)))
        self.assertEqual(summary["pg"]["carried"], len(self.proved(third) - self.ran(third)))
        self.assertTrue(proven <= set(n for row in self.receipts(third).values() for n in row["carried"]))
        audited, _ = PlainRun.shards_audit(self, third)
        self.assertEqual(json.loads(audited.stdout)["union"], "exact")

    def test_a_chain_of_three_interrupted_links_keeps_every_links_names(self):
        first = self.stopped_run()
        proven = self.proved(first)
        _, second = self.drive("--resume", str(first), die="b03")
        self.lose_a_receipts(second)
        _, third = self.drive("--resume", str(second), die="b04")
        self.lose_a_receipts(third)
        survivors = self.proved(third) - set(n for row in self.receipts(third).values() for n in row["carried"]
                                             if "--lib" not in row["args"])
        _, fourth = self.drive("--resume", str(third), check=0)
        self.assertEqual(self.ran(fourth) & proven, set())
        self.assertEqual(self.ran(fourth) & (self.proved(third) - set(A_NAMES)), set())
        self.assertEqual(self.summary(fourth)["pg"]["union"], "exact")
        self.assertTrue(survivors <= self.proved(fourth))
        runs = {r["run"] for row in self.receipts(fourth).values() for r in row["carried"].values()}
        self.assertIn(json.loads((first / "manifest.json").read_text())["run_id"], runs)

    def test_the_failure_history_survives_a_link_without_receipts(self):
        self.set_register([{"test": "a03", "cargo": CARGO, "issue": 1, "expires": Resume.soon(self)}])
        _, first = self.drive(fail=["a03"])
        _, second = self.drive("--resume", str(first))
        self.lose_a_receipts(second)
        _, third = self.drive("--resume", str(second), check=0)
        counts = json.loads((third / "manifest.json").read_text())["failed"]["pg"]
        self.assertEqual([c["a03"] for c in counts.values() if "a03" in c], [1])

    def snapshot_case(self, mutate, fragment):
        first = self.stopped_run()
        _, second = self.drive("--resume", str(first), check=0)
        mutate(second)
        result, output = self.drive("--resume", str(second))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(fragment, result.stderr)
        self.assertFalse(output.exists(), "a refused resume creates no output")

    def rewrite(self, directory, **changes):
        """Change resume-input.json and keep the manifest's digest of it honest."""
        path = directory / "resume-input.json"
        snapshot = json.loads(path.read_text())
        snapshot.update(changes)
        path.write_text(json.dumps(snapshot))
        manifest_path = directory / "manifest.json"
        manifest = json.loads(manifest_path.read_text())
        manifest["resumed_from"]["snapshot_sha256"] = hashlib.sha256(path.read_bytes()).hexdigest()
        manifest_path.write_text(json.dumps(manifest))

    def test_a_tampered_or_mismatched_inherited_snapshot_is_refused(self):
        def add_name(directory):
            path = directory / "resume-input.json"
            snapshot = json.loads(path.read_text())
            next(iter(snapshot["keys"]["pg"].values()))["ok"]["a99"] = {"status": "ok", "seconds": 1, "run": "x"}
            path.write_text(json.dumps(snapshot))  # the manifest's digest no longer matches

        def keys_with(directory, edit):
            snapshot = json.loads((directory / "resume-input.json").read_text())
            edit(next(iter(snapshot["keys"]["pg"].values())))
            self.rewrite(directory, keys=snapshot["keys"])

        cases = {
            "contents changed": (add_name, "does not match its digest"),
            "snapshot missing": (lambda d: (d / "resume-input.json").unlink(), "unreadable resume snapshot"),
            "snapshot truncated": (lambda d: (d / "resume-input.json").write_text('{"keys"'), "does not match its digest"),
            "no digest in the manifest": (lambda d: self.edit_manifest_link(d, snapshot_sha256=None), "no digest"),
            "malformed after a matching digest": (lambda d: self.rewrite(d, keys=[]), "malformed resume snapshot"),
            "snapshot of another run": (lambda d: self.rewrite(d, run_id="f" * 32), "another run"),
            "snapshot of another head": (lambda d: self.rewrite(d, tree="0" * 40), "another head"),
            "snapshot of other inputs": (lambda d: self.rewrite(d, inputs="1" * 64), "another head"),
            "ok name outside the list": (lambda d: keys_with(d, lambda e: e["ok"].update(
                zz={"status": "ok", "seconds": 1, "run": "x"})), "malformed resume snapshot"),
            "ok status that is not ok": (lambda d: keys_with(d, lambda e: e["ok"].update(
                {e["listed"][0]: {"status": "FAILED", "seconds": 1, "run": "x"}})), "malformed resume snapshot"),
            "list that disagrees with the receipts": (lambda d: keys_with(d, lambda e: e["listed"].append("zz")),
                                                      "disagree"),
        }
        for name, (mutate, fragment) in cases.items():
            with self.subTest(name):
                self.snapshot_case(mutate, fragment)

    def edit_manifest_link(self, directory, **changes):
        path = directory / "manifest.json"
        manifest = json.loads(path.read_text())
        manifest["resumed_from"].update(changes)
        path.write_text(json.dumps(manifest))

    def test_a_receipt_carrying_what_the_snapshot_never_proved_is_refused(self):
        first = self.stopped_run()
        _, second = self.drive("--resume", str(first), check=0)
        for path, row in self.receipts(second).items():
            if "--lib" in row["args"]:  # every shard agrees, so only the snapshot can contradict them
                row["carried"]["a98"] = {"status": "ok", "seconds": 1, "run": "x"}
                path.write_text(json.dumps(row))
        result, _ = self.drive("--resume", str(second))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("never proved", result.stderr)


class Tls(Harness):
    """A resume that carries a TLS case must still finish through the real helper."""

    TLS_NAMES = {"tls_verified", "tls_untrusted"}
    TOTAL = len(A_NAMES) + len(B_NAMES) + 4

    def setUp(self):
        (self.repo / "scripts/live-tests-pg.sh").write_text(RUNNER_TLS)
        (self.repo / "scripts/tls_case.py").write_text(TLS_CASE)
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", "tls runner")

    def tearDown(self):
        (self.repo / "scripts/live-tests-pg.sh").write_text(RUNNER)
        (self.repo / "scripts/tls_case.py").unlink()
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", "plain runner")
        super().tearDown()

    def stopped_after_tls(self):
        # The TLS cases run before `second`, so a death in `second` leaves them proven.
        output = self.stopped_run(die="b03")
        proven = {n for row in self.receipts(output).values() if row["valid"]
                  for n, r in row["reported"].items() if r["status"] == "ok"}
        self.assertTrue(self.TLS_NAMES <= proven, "the first run should have proven the TLS cases")
        return output

    def resumed(self):
        stopped = self.stopped_after_tls()
        _, output = self.drive("--resume", str(stopped), check=0)
        return output

    def test_a_resume_that_carried_the_tls_cases_finishes_and_audits_exactly(self):
        output = self.resumed()
        summary = self.summary(output)
        self.assertEqual((summary["pg"]["union"], summary["pg"]["passed"]), ("exact", self.TOTAL))
        carried = {n for row in self.receipts(output).values() for n in row["carried"]}
        self.assertTrue(self.TLS_NAMES <= carried)
        audited, _ = PlainRun.shards_audit(self, output)
        self.assertEqual(json.loads(audited.stdout)["union"], "exact")

    def test_a_run_without_resume_prints_no_attestation_and_finishes(self):
        _, output = self.drive(check=0)
        self.assertEqual(self.summary(output)["pg"]["passed"], self.TOTAL)
        self.assertNotIn(self.shards.CARRIED_LINE, "".join(p.read_text() for p in output.glob("pg/*.log")))

    def attested(self):
        """A genuine resume's receipts, the environment its shard ran in, and the proxy's lines."""
        output = self.resumed()
        directory = output / "pg/0"
        keys = {}
        for path, row in self.receipts(output).items():
            if path.parent.name == "0" and "--test" in row["args"] and "tls" in row["args"]:
                keys.setdefault(row["args"][row["args"].index("--exact") + 1], []).append(path.stem)
        manifest = json.loads((output / "manifest.json").read_text())
        env = {"PBPS_LIVE_RESUME": str(output / "resume-input.json"), "PBPS_LIVE_SHARD_DIR": str(directory),
               "PBPS_LIVE_TREE": manifest["tree"], "PBPS_LIVE_INPUTS": manifest["inputs"], "PBPS_LIVE_SUITE": "pg"}
        return directory, {n: sorted(k) for n, k in keys.items()}, env

    def transport(self):
        return load_shards(self.repo / "scripts/live-transport.py")

    def run_case(self, transport, env, output, test="tls_verified", code=0):
        """The real `run_case` against a `cargo` that only prints `output` and succeeds."""
        fake = self.runs / "fake-cargo"
        fake.mkdir(exist_ok=True)
        (fake / "cargo").write_text("#!/bin/sh\nprintf '%s' \"$ECHO_OUT\"\nexit ${ECHO_RC:-0}\n")
        (fake / "cargo").chmod(0o755)
        process_env = dict(os.environ, ECHO_OUT=output, ECHO_RC=str(code), PATH=f"{fake}{os.pathsep}{os.environ['PATH']}", **env)
        with contextlib.redirect_stdout(io.StringIO()):
            transport.run_case(["cargo", "test", "-p", "demo", "--test", "tls", "--"], test, process_env)

    def line(self, key):
        return f"shard 0/2 {key}: 0/1 tests\n{self.shards.CARRIED_LINE}{key}\n"

    def test_the_helper_accepts_only_what_the_proxy_attests_and_the_receipts_confirm(self):
        directory, keys, env = self.attested()
        verified = keys["tls_verified"][0]
        self.run_case(self.transport(), env, self.line(verified))  # a genuine attestation is accepted
        refused = {
            "silent proxy": (env, ""),
            "the shard line only": (env, f"shard 0/2 {verified}: 0/1 tests\n"),
            "attestation without a resumed run": ({k: v for k, v in env.items() if k != "PBPS_LIVE_RESUME"},
                                                  self.line(verified)),
            "key of another case": (env, self.line(keys["tls_untrusted"][0])),
            "key that names no receipt": (env, self.line("0" * 24 + "-0")),
            "key of another occurrence": (env, self.line(verified.rsplit("-", 1)[0] + "-1")),
        }
        for name, (variant, output) in refused.items():
            with self.subTest(name):
                with self.assertRaisesRegex(RuntimeError, "exactly once"):
                    self.run_case(self.transport(), variant, output)
        with self.subTest("attested but the proxy failed"):
            with self.assertRaisesRegex(RuntimeError, "exactly once"):
                self.run_case(self.transport(), env, self.line(verified), code=1)
        with self.subTest("the verifier alone, without a resume"):
            args = ["test", "-p", "demo", "--test", "tls", "--", "--ignored", "--exact", "tls_verified",
                    "--test-threads=1", "--format=pretty", "--color=never"]
            self.assertEqual(self.shards.carried_attestation(self.line(verified), args, env, 0), ["tls_verified"])
            bare = {k: v for k, v in env.items() if k != "PBPS_LIVE_RESUME"}
            self.assertIsNone(self.shards.carried_attestation(self.line(verified), args, bare, 0))

    def test_a_receipt_that_does_not_confirm_the_attestation_is_refused(self):
        directory, keys, env = self.attested()
        verified = keys["tls_verified"][0]
        path = directory / (verified + ".json")
        original = json.loads(path.read_text())
        changes = {
            "nothing carried": {"carried": {}},
            "the case ran here": {"assigned": ["tls_verified"]},
            "another command": {"args": original["args"] + ["--extra"]},
            "another head": {"tree": "0" * 40},
            "other inputs": {"inputs": "1" * 64},
            "an invalid row": {"valid": False},
        }
        for name, change in changes.items():
            with self.subTest(name):
                path.write_text(json.dumps({**original, **change}))
                with self.assertRaisesRegex(RuntimeError, "exactly once"):
                    self.run_case(self.transport(), env, self.line(verified))
        path.write_text(json.dumps(original))

    def test_each_occurrence_of_a_repeated_case_needs_its_own_attestation(self):
        directory, keys, env = self.attested()
        first, second, third = keys["tls_untrusted"]
        transport = self.transport()
        self.run_case(transport, env, self.line(first), "tls_untrusted")
        with self.assertRaisesRegex(RuntimeError, "exactly once"):
            self.run_case(transport, env, self.line(first), "tls_untrusted")  # the first one's evidence again
        transport = self.transport()
        for key in (first, second, third):
            self.run_case(transport, env, self.line(key), "tls_untrusted")


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
