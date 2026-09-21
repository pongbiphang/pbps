#!/usr/bin/env python3
"""ADR-0017 feasibility evidence, not the production compose implementation."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import subprocess
import tempfile
import unittest


class IsolatedCompose(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory(prefix="pbps-isolated-compose-")
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.repo = self.root / "source"
        self.snapshot = self.root / "candidate"
        self.remote = self.root / "remote.git"
        self.repo.mkdir()
        self.snapshot.mkdir()
        (self.repo / "schema").mkdir()
        hooks = self.root / "empty-hooks"
        hooks.mkdir()
        self.env = {k: v for k, v in os.environ.items() if not k.startswith("GIT_")}
        self.env.update(
            GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
            GIT_TERMINAL_PROMPT="0", GIT_AUTHOR_NAME="Compose experiment",
            GIT_AUTHOR_EMAIL="compose@example.invalid",
            GIT_COMMITTER_NAME="Compose experiment",
            GIT_COMMITTER_EMAIL="compose@example.invalid",
        )
        self.command = ["git", "--no-replace-objects", "--literal-pathspecs",
                        "-c", "core.hooksPath=" + str(hooks),
                        "-c", "core.fsmonitor=false"]
        self.original = (
            b'table: dbo.customer\ncolumns:\n'
            b'  id: {type: int, nullable: false}\n'
            b'  customer_name: {type: "nvarchar(100)", nullable: true}\n'
            b'primary_key: [id]\n'
        )
        (self.repo / "pbps.yml").write_bytes(
            b'dialect: mssql\nenvironments:\n'
            b'  unconfigured:\n    url_env: PBPS_COMPOSE_PROBE_UNSET\n'
        )
        self.schema = self.repo / "schema/customer.yml"
        self.schema.write_bytes(self.original)
        (self.repo / "unrelated.txt").write_bytes(b"base\n")
        self.cli(self.repo, "plan")
        self.git("init", "-q", "-b", "main")
        self.git("config", "commit.gpgSign", "false")
        self.git("add", "-A")
        self.git("commit", "-qm", "base")
        self.base = self.oid("rev-parse", "HEAD")
        self.git("init", "--bare", "-q", str(self.remote))
        self.git("push", str(self.remote), self.base + ":refs/heads/main")
        self.schema.write_bytes(self.original.replace(b"customer_name", b"full_name"))
        (self.repo / "unrelated.txt").write_bytes(b"unrelated staged work\n")
        self.git("add", "unrelated.txt")
        (self.repo / "untracked.txt").write_bytes(b"untracked work\n")
        self.branch = "refs/heads/pbps-compose/experiment"

    def run_command(self, args, *, cwd=None, data=None, extra_env=None, ok=True):
        result = subprocess.run(
            args, cwd=cwd or self.repo, input=data, capture_output=True,
            env={**self.env, **(extra_env or {})}, timeout=30,
        )
        if ok:
            self.assertEqual(result.returncode, 0, result.stderr.decode(errors="replace"))
        return result

    def git(self, *args, **kwargs):
        return self.run_command([*self.command, *args], **kwargs)

    def oid(self, *args, **kwargs):
        return self.git(*args, **kwargs).stdout.decode().strip()

    def cli(self, project, *args):
        return self.run_command(
            [OPTIONS.pbps, "--project", str(project), *args, "--no-input"], cwd=project
        )

    def source_state(self):
        return {
            "head": (self.repo / ".git/HEAD").read_bytes(),
            "tip": self.oid("rev-parse", "HEAD"),
            "index": (self.repo / ".git/index").read_bytes(),
            "files": {
                str(p.relative_to(self.repo)): (p.read_bytes(), p.stat().st_mode)
                for p in self.repo.rglob("*")
                if p.is_file() and ".git" not in p.relative_to(self.repo).parts
            },
        }

    def capture(self):
        # This controlled fixture has regular files only. Production must verify
        # containment, membership, modes and read failures before using these bytes.
        for row in self.git("ls-tree", "-rz", self.base).stdout.split(b"\0"):
            if not row:
                continue
            header, path = row.split(b"\t", 1)
            mode, kind, oid = header.split()
            self.assertEqual(kind, b"blob")
            self.assertIn(mode, (b"100644", b"100755"))
            target = self.snapshot / os.fsdecode(path)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(self.git("cat-file", "blob", oid.decode()).stdout)
            target.chmod(0o755 if mode == b"100755" else 0o644)
        captured = self.schema.read_bytes()
        (self.snapshot / "schema/customer.yml").write_bytes(captured)
        self.input_digest = hashlib.sha256(captured).hexdigest()
        self.cli(self.snapshot, "rename", "dbo.customer.customer_name", "full_name")
        self.cli(self.snapshot, "validate")
        index_env = {"GIT_INDEX_FILE": str(self.root / "candidate.index")}
        self.git("read-tree", self.base, extra_env=index_env)
        for path in ("schema/customer.yml", "schema.ids.json"):
            blob = self.oid("hash-object", "-w", "--no-filters", "--stdin",
                            data=(self.snapshot / path).read_bytes())
            self.git("update-index", "--add", "--cacheinfo", "100644," + blob + "," + path,
                     extra_env=index_env)
        self.tree = self.oid("write-tree", extra_env=index_env)
        self.diff = self.git("diff", "--no-ext-diff", "--no-textconv", self.base, self.tree).stdout

    def make_commit(self):
        return self.oid("commit-tree", self.tree, "-p", self.base, "-m", "rename customer_name full_name")

    def create_output(self, commit):
        if OPTIONS.revert_ref_type_guard:
            return self.git("update-ref", "--no-deref", self.branch, commit,
                            "0" * len(commit), ok=False).returncode == 0
        process = subprocess.Popen(
            [*self.command, "update-ref", "--stdin"], cwd=self.repo, env=self.env,
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            text=True,
        )
        def exchange(line):
            process.stdin.write(line + "\n")
            process.stdin.flush()
            self.assertTrue(select.select([process.stdout], [], [], 10)[0], line)
            return process.stdout.readline().strip()
        try:
            self.assertEqual(exchange("start"), "start: ok")
            process.stdin.write("option no-deref\ncreate " + self.branch + " " + commit + "\n")
            if exchange("prepare") != "prepare: ok":
                return False
            # Git 2.43's expected-zero CAS alone overwrites a dangling symref.
            # Inspect the live ref's type while the prepared transaction holds it.
            kind = self.git("symbolic-ref", "-q", "--no-recurse", self.branch, ok=False)
            self.assertIn(kind.returncode, (0, 1), kind.stderr)
            checked_out = ("branch " + self.branch).encode() in self.git(
                "worktree", "list", "--porcelain", "-z"
            ).stdout.split(b"\0")
            accepted = kind.returncode == 1 and not checked_out
            action = "commit" if accepted else "abort"
            self.assertEqual(exchange(action), action + ": ok")
            process.stdin.close()
            self.assertEqual(process.wait(timeout=10), 0)
            return accepted
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=10)
            for pipe in (process.stdin, process.stdout, process.stderr):
                pipe.close()

    def push(self, commit, *, destination=None):
        return self.git(
            "push", "--no-verify", "--no-follow-tags", "--recurse-submodules=no",
            "--force-with-lease=" + self.branch + ":", str(destination or self.remote),
            commit + ":" + self.branch, ok=False,
        )

    def test_candidate_excludes_unrelated_work_and_preserves_source(self):
        before = self.source_state()
        self.capture()
        commit = self.make_commit()
        self.assertTrue(self.create_output(commit))
        self.assertEqual(self.push(commit).returncode, 0)
        self.assertEqual(self.source_state(), before)
        self.assertEqual(self.oid("rev-parse", commit + "^{tree}"), self.tree)
        self.assertEqual(self.oid("rev-parse", commit + "^"), self.base)
        self.assertEqual(set(self.oid("diff-tree", "--no-commit-id", "--name-only", "-r", commit).splitlines()),
                         {"schema/customer.yml", "schema.ids.json"})
        self.assertEqual(self.git("show", commit + ":unrelated.txt").stdout, b"base\n")
        self.assertNotIn(b"full_name", (self.repo / "schema.ids.json").read_bytes())
        self.assertIn(b"full_name", self.git("show", commit + ":schema.ids.json").stdout)
        # Continuing in a separately selected checkout needs no copy-back or ids merge.
        result = self.root / "result-workspace"
        self.git("worktree", "add", "-q", str(result), self.branch.removeprefix("refs/heads/"))
        self.cli(result, "validate")
        self.assertEqual((result / "schema.ids.json").read_bytes(), (self.snapshot / "schema.ids.json").read_bytes())
        self.assertEqual(self.source_state(), before)

    def test_edit_after_capture_is_excluded_from_reviewed_tree(self):
        self.capture()
        reviewed_tree, reviewed_diff = self.tree, self.diff
        self.schema.write_bytes(self.schema.read_bytes() + b"description: later edit\n")
        edited = self.source_state()
        if OPTIONS.revert_candidate_freeze:
            self.capture()
        commit = self.make_commit()
        self.assertEqual(self.oid("rev-parse", commit + "^{tree}"), reviewed_tree)
        self.assertEqual(self.git("diff", "--no-ext-diff", "--no-textconv", self.base, commit).stdout, reviewed_diff)
        self.assertEqual(self.source_state(), edited)

    def test_existing_direct_destination_is_not_overwritten(self):
        self.capture()
        self.git("update-ref", self.branch, self.base)
        self.assertFalse(self.create_output(self.make_commit()))
        self.assertEqual(self.oid("rev-parse", self.branch), self.base)

    def test_dangling_symbolic_destination_is_not_overwritten(self):
        self.capture()
        target = "refs/heads/missing"
        self.git("symbolic-ref", self.branch, target)
        self.assertFalse(self.create_output(self.make_commit()))
        self.assertEqual(self.oid("symbolic-ref", self.branch), target)

    def test_unborn_checked_out_destination_is_refused(self):
        self.capture()
        sibling = self.root / "sibling"
        self.git("worktree", "add", "--detach", "-q", str(sibling), self.base)
        self.git("symbolic-ref", "HEAD", self.branch, cwd=sibling)
        self.assertFalse(self.create_output(self.make_commit()))
        self.assertNotEqual(self.git("show-ref", "--verify", self.branch, ok=False).returncode, 0)

    def test_narrow_push_also_exports_unpublished_ancestors(self):
        self.capture()
        unpublished = self.make_commit()
        advertised = self.oid("ls-remote", str(self.remote), "refs/heads/main").split()[0]
        self.assertEqual(advertised, self.base)
        self.assertNotEqual(advertised, unpublished)
        # A narrow push really does export the ancestor: the guard is necessary.
        next_commit = self.oid("commit-tree", self.tree, "-p", unpublished, "-m", "next")
        self.assertEqual(self.push(next_commit).returncode, 0)
        self.git("cat-file", "-e", unpublished + "^{commit}", cwd=self.remote)

    def test_signer_failure_does_not_publish_a_branch(self):
        self.capture()
        before = self.source_state()
        signer = self.root / "refuse-signing"
        signer.write_text("#!/bin/sh\nexit 1\n")
        signer.chmod(0o700)
        failed = self.git("-c", "gpg.program=" + str(signer), "-c", "gpg.format=openpgp",
                          "commit-tree", "-S", self.tree, "-p", self.base, "-m", "signed", ok=False)
        self.assertNotEqual(failed.returncode, 0)
        self.assertEqual(self.source_state(), before)
        self.assertNotEqual(self.git("show-ref", "--verify", self.branch, ok=False).returncode, 0)

    def test_failed_push_keeps_local_commit(self):
        self.capture()
        commit = self.make_commit()
        self.assertTrue(self.create_output(commit))
        self.assertNotEqual(self.push(commit, destination=self.root / "missing.git").returncode, 0)
        self.assertEqual(self.oid("rev-parse", self.branch), commit)
        self.assertEqual(self.push(commit).returncode, 0)
        self.assertEqual(self.oid("ls-remote", str(self.remote), self.branch).split()[0], commit)

    def test_lost_push_acknowledgment_can_be_reconciled_without_new_commit(self):
        self.capture()
        commit = self.make_commit()
        self.assertTrue(self.create_output(commit))
        # Real Git completes the push; a wrapper discards its success and exits
        # unsuccessfully. This models lost caller evidence, not a network fault.
        wrapper = self.root / "discard-ack.py"
        wrapper.write_text(
            "import subprocess,sys\n"
            "r=subprocess.run(sys.argv[1:], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
            "sys.exit(91 if r.returncode == 0 else r.returncode)\n"
        )
        import sys
        ack = self.run_command([sys.executable, str(wrapper), *self.command, "push",
                                "--force-with-lease=" + self.branch + ":", str(self.remote),
                                commit + ":" + self.branch], ok=False)
        self.assertEqual(ack.returncode, 91)
        self.assertEqual(self.oid("rev-parse", self.branch), commit)
        self.assertEqual(self.oid("ls-remote", str(self.remote), self.branch).split()[0], commit)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pbps", required=True, type=lambda p: str(Path(p).resolve()))
    parser.add_argument("--revert-ref-type-guard", action="store_true")
    parser.add_argument("--revert-candidate-freeze", action="store_true")
    OPTIONS = parser.parse_args()
    print(json.dumps({"git": subprocess.check_output(["git", "--version"], text=True).strip(),
                      "pbps": subprocess.check_output([OPTIONS.pbps, "--version"], text=True).strip(),
                      "revert_ref_type_guard": OPTIONS.revert_ref_type_guard,
                      "revert_candidate_freeze": OPTIONS.revert_candidate_freeze}), flush=True)
    unittest.main(argv=[__file__], verbosity=2)
