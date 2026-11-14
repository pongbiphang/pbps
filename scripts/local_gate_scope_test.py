#!/usr/bin/env python3
"""Tests for scripts/local_gate_scope.py. Run: python3 scripts/local_gate_scope_test.py"""

import contextlib
import io
import json
import posixpath
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import gate_scope  # noqa: E402
import local_gate_scope as lgs  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts" / "local_gate_scope.py"
GIT = ["git", "-c", "user.name=t", "-c", "user.email=t@t"]
TREE = {
    "crates/pbps-model/src/lib.rs": "// code\n",
    "crates/pbps-model/Cargo.toml": "[package]\n",
    "Cargo.toml": "[workspace]\n",
    "Cargo.lock": "# lock\n",
    "rust-toolchain.toml": "[toolchain]\n",
    "docs/SPEC.md": "spec\n",
    "README.md": "readme\n",
    ".github/workflows/ci.yml": "name: ci\n",
    "scripts/pr-size.py": "# tool\n",
    "scripts/pr_size_test.py": "# test\n",
    "scripts/live-tests.sh": "#!/bin/sh\n",
    "scripts/live-tests-pg.sh": "#!/bin/sh\n",
    "scripts/fixtures/pg-history695/a.sql": "select 1;\n",
    "schemas/envelope.schema.json": "{}\n",
}
# One sample per KEEP_FULL entry, in order. Each must be held back.
KEEP_FULL_SAMPLES = [
    "scripts/live-tests.sh", "scripts/check-live-fixture.py",
    "scripts/resolver-mssql-fixtures.sh", "scripts/compatibility-matrix.json",
    "scripts/fixtures/pg-history695/a.sql", "scripts/gate_rerun.py",
    "scripts/local_gate_scope.py", "schemas/envelope.schema.json", "docs/measurements/partition-moves-2296.md",
    "crates/pbps-cli/src/lib.rs", "crates/pbps-cli/Cargo.toml", "rust-toolchain.toml",
]
# What the rule says about each pattern of the unchanged-patch rule's BLOCKED.
# `gate_scope.BLOCKED` asks "could a rebase change this patch's meaning", which
# a workflow or register edit can; this rule asks "does a live run read it".
BLOCKED_SAMPLES = {
    r"(^|/)Cargo\.(toml|lock)$": ("crates/pbps-pg/Cargo.toml", "full"),
    r"^rust-toolchain(\.toml)?$": ("rust-toolchain.toml", "full"),
    r"^\.cargo/": (".cargo/config.toml", "full"),
    r"^\.github/": (".github/workflows/ci.yml", "non-live"),
    r"^scripts/live[-_]": ("scripts/live-shards.py", "full"),
    r"^scripts/check-live-fixture": ("scripts/check-live-fixture.py", "full"),
    r"^scripts/resolver-mssql-fixtures\.sh$": ("scripts/resolver-mssql-fixtures.sh", "full"),
    r"^scripts/fixtures/": ("scripts/fixtures/pg-history695/a.control", "full"),
    r"^scripts/(gate_scope|gate_rerun|known_flakes|known-flakes)": ("scripts/gate_rerun.py", "full"),
}
# Files a live runner names that the rule frees on purpose, with the measured why.
FREED_BUT_NAMED = {
    "known-flakes.json": "read from `--flakes-ref` (default origin/master), never from the head",
    "ignored_test_inventory.py": "imported only by python_checks and cargo_check, the non-live suites",
}
# Rust tests may read these docs at run time because each reader is an ordinary
# `#[test]` that the non-live gate runs; a new doc read needs the same check. A
# doc compiled in with `include_str!` is never on this list (docs/measurements/).
KNOWN_DOC_READS = {"docs/STATUS.md", "docs/ORDERING.md"}


def scope(*paths):
    return lgs.decide(paths)[0]


class Decide(unittest.TestCase):
    def test_scripts_only_change_is_non_live(self):
        self.assertEqual(scope("scripts/pr-size.py", "scripts/pr_size_test.py"), "non-live")

    def test_documentation_only_change_is_non_live(self):
        self.assertEqual(scope("docs/decisions/process.md", "AGENTS.md", "README.md"), "non-live")

    def test_workflow_only_change_is_non_live(self):
        self.assertEqual(scope(".github/workflows/ci.yml"), "non-live")

    def test_register_and_runner_tests_are_free_because_no_live_run_reads_them(self):
        self.assertEqual(scope("scripts/known-flakes.json", "scripts/live_test_runners_test.py",
                               "scripts/live_shards_resume_test.py"), "non-live")

    def test_every_keep_full_entry_holds_a_head_back(self):
        self.assertEqual(len(KEEP_FULL_SAMPLES), len(lgs.KEEP_FULL))
        for pattern, sample in zip(lgs.KEEP_FULL, KEEP_FULL_SAMPLES):
            with self.subTest(pattern=pattern[0].pattern):
                self.assertRegex(sample, pattern[0])
                self.assertEqual(scope(sample), "full")

    def test_lookalike_paths_are_not_live_inputs(self):
        for path in ("scripts/live_test_runners_test.py", "scripts/check_live_fixture_test.py",
                     "scripts/known_flakes_test.py", "scripts/pre-live-tests.sh",
                     "scripts/ci_release_scope_test.py", "docs/live-tests.md", "scripts/gate_scope_test.py"):
            with self.subTest(path=path):
                self.assertEqual(scope(path), "non-live")

    def test_lookalike_paths_outside_a_class_are_not_free_either(self):
        for path in ("scripts/sub/live-tests.sh", "docs2/a.md", "notes/crates/x.rs", "scripts"):
            with self.subTest(path=path):
                self.assertEqual(scope(path), "full")

    def test_a_path_in_no_class_is_full(self):
        for path in ("NEW.txt", "deny.toml", "spikes/pg-driver/src/main.rs", "tests/ui-browser/fixture.mjs",
                     ".gitattributes", "scripts/ci-event-policy/package.json"):
            with self.subTest(path=path):
                verdict, reason, _ = lgs.decide([path])
                self.assertEqual(verdict, "full")
                self.assertIn("no class", reason)

    def test_a_mixed_change_is_full_and_names_only_the_held_path(self):
        verdict, reason, _ = lgs.decide(["docs/a.md", "scripts/pr-size.py", "crates/pbps-pg/src/lib.rs"])
        self.assertEqual(verdict, "full")
        self.assertIn("crates/pbps-pg/src/lib.rs", reason)
        self.assertNotIn("docs/a.md", reason)

    def test_an_empty_diff_is_full(self):
        self.assertEqual(lgs.decide([])[0], "full")

    def test_a_crate_markdown_file_is_a_crate_file_not_documentation(self):
        for path in ("crates/pbps-cli/tests/fixtures/published-schemas/README.md",
                     "crates/pbps-docs/tests/fixtures/without-types.md", "crates/pbps-pg/README.md"):
            with self.subTest(path=path):
                verdict, reason, _ = lgs.decide([path])
                self.assertEqual(verdict, "full")
                self.assertIn("crate source", reason)

    def test_a_document_compiled_into_a_test_binary_is_full(self):
        self.assertEqual(scope("docs/measurements/enum-rebuild-1635.md"), "full")
        self.assertEqual(scope("docs/measurements/partition-moves-2296.md", "docs/SPEC.md"), "full")

    def test_a_symlink_or_submodule_is_full_even_under_a_free_name(self):
        verdict, reason, _ = lgs.decide(["docs/a.md", "docs/link.md"], indirect={"docs/link.md"})
        self.assertEqual(verdict, "full")
        self.assertIn("symlink or submodule", reason)
        self.assertIn("docs/link.md", reason)
        self.assertNotIn("docs/a.md", reason)

    def test_a_long_list_of_held_paths_is_shortened_in_the_reason(self):
        _, reason, _ = lgs.decide([f"crates/c/src/f{i}.rs" for i in range(9)])
        self.assertIn("and 6 more", reason)
        self.assertNotIn("f8.rs", reason)

    def test_the_rule_and_its_scope_siblings_keep_the_full_gate(self):
        for path in ("scripts/local_gate_scope.py", "scripts/ci_release_scope.py", "scripts/gate_scope.py"):
            self.assertEqual(scope(path), "full", path)

    def test_every_unchanged_patch_blocked_pattern_has_a_decided_counterpart(self):
        self.assertEqual({p.pattern for p in gate_scope.BLOCKED}, set(BLOCKED_SAMPLES))
        for pattern, (sample, expected) in BLOCKED_SAMPLES.items():
            with self.subTest(pattern=pattern):
                self.assertRegex(sample, pattern)
                self.assertEqual(scope(sample), expected)


class Repo(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="pbps-local-scope-")
        self.addCleanup(self.directory.cleanup)
        self.path = Path(self.directory.name)
        self.git("init", "-q", "-b", "main")
        for name, text in TREE.items():
            self.write(name, text)
        self.base = self.commit("base")

    def git(self, *args):
        return subprocess.run([*GIT, *args], cwd=self.path, text=True, capture_output=True, check=True).stdout.strip()

    def write(self, name, text):
        target = self.path / name
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(text)

    def commit(self, message):
        self.git("add", "-A")
        self.git("commit", "-q", "--allow-empty", "-m", message)
        return self.git("rev-parse", "HEAD")

    def commit_index(self, message):
        """Commit the index as it is: `add -A` would restage the work tree over it."""
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def change(self, files, start=None):
        """A commit on `start` (default base) where `files` maps path -> text (None deletes)."""
        self.git("checkout", "-q", "--detach", start or self.base)
        for name, text in files.items():
            if text is None:
                (self.path / name).unlink()
            else:
                self.write(name, text)
        return self.commit("change")

    def verdict(self, head, base=None):
        return lgs.verdict(self.path, base or self.base, head)


class Verdict(Repo):
    def test_a_scripts_only_head_is_non_live(self):
        result = self.verdict(self.change({"scripts/pr-size.py": "# changed\n"}))
        self.assertEqual(result["verdict"], "non-live", result)
        self.assertEqual(result["classes"], {"script outside the live closure": 1})
        self.assertRegex(result["merge_base"], "^[0-9a-f]{40}$")

    def test_a_documentation_only_head_is_non_live(self):
        self.assertEqual(self.verdict(self.change({"docs/SPEC.md": "new\n", "docs/new.md": "x\n"}))["verdict"], "non-live")

    def test_a_crate_change_is_full(self):
        result = self.verdict(self.change({"crates/pbps-model/src/lib.rs": "// edited\n"}))
        self.assertEqual(result["verdict"], "full")
        self.assertIn("crate source and its test fixtures: crates/pbps-model/src/lib.rs", result["reason"])

    def test_a_manifest_lockfile_or_toolchain_change_is_full_and_says_so(self):
        for name, why in (("Cargo.toml", "manifest"), ("Cargo.lock", "manifest"),
                          ("crates/pbps-model/Cargo.toml", "crate source"), ("rust-toolchain.toml", "toolchain")):
            with self.subTest(name=name):
                result = self.verdict(self.change({name: "edited\n"}))
                self.assertEqual(result["verdict"], "full")
                self.assertIn(why, result["reason"])

    def test_a_live_runner_or_fixture_change_is_full(self):
        for name in ("scripts/live-tests.sh", "scripts/live-tests-pg.sh", "scripts/fixtures/pg-history695/a.sql",
                     "schemas/envelope.schema.json"):
            with self.subTest(name=name):
                result = self.verdict(self.change({name: "edited\n"}))
                self.assertEqual(result["verdict"], "full")
                self.assertIn(name, result["reason"])

    def test_a_rename_out_of_a_live_input_is_full_on_the_old_name(self):
        self.git("checkout", "-q", "--detach", self.base)
        self.git("mv", "scripts/live-tests.sh", "scripts/helper-tool.sh")
        result = self.verdict(self.commit("rename"))
        self.assertEqual(result["verdict"], "full")
        self.assertIn("scripts/live-tests.sh", result["reason"])

    def test_a_rename_into_a_live_input_is_full_on_the_new_name(self):
        self.git("checkout", "-q", "--detach", self.base)
        self.git("mv", "scripts/pr-size.py", "scripts/live-helper.py")
        result = self.verdict(self.commit("rename"))
        self.assertEqual(result["verdict"], "full")
        self.assertIn("scripts/live-helper.py", result["reason"])

    def test_the_diff_is_taken_against_the_merge_base_not_the_moved_base(self):
        head = self.change({"docs/SPEC.md": "branch\n"})
        moved = self.change({"crates/pbps-model/src/lib.rs": "// master moved\n"})
        result = self.verdict(head, base=moved)
        self.assertEqual(result["verdict"], "non-live", result)
        self.assertEqual(result["merge_base"], self.base)

    def test_an_unreadable_ref_is_full(self):
        head = self.change({"docs/SPEC.md": "x\n"})
        for base, tip in (("no-such-ref", head), (self.base, "no-such-ref")):
            with self.subTest(base=base, head=tip):
                result = lgs.verdict(self.path, base, tip)
                self.assertEqual(result["verdict"], "full")
                self.assertIn("unknown is not unaffected", result["reason"])

    def test_a_head_with_no_merge_base_is_full(self):
        self.git("checkout", "-q", "--orphan", "other")
        self.git("rm", "-rq", "--cached", ".")
        self.write("docs/only.md", "x\n")
        orphan = self.commit("orphan")
        result = self.verdict(orphan)
        self.assertEqual(result["verdict"], "full")
        self.assertIn("merge-base", result["reason"])

    def test_a_directory_that_is_not_a_repository_is_full(self):
        with tempfile.TemporaryDirectory(prefix="pbps-local-scope-none-") as empty:
            self.assertEqual(lgs.verdict(empty, "origin/master", "HEAD")["verdict"], "full")
        self.assertEqual(lgs.verdict(self.path / "missing", "a", "b")["verdict"], "full")

    def test_a_new_symlink_under_a_free_name_is_full(self):
        self.git("checkout", "-q", "--detach", self.base)
        (self.path / "docs/link.md").symlink_to("../crates/pbps-model/src/lib.rs")
        result = self.verdict(self.commit("link"))
        self.assertEqual(result["verdict"], "full")
        self.assertIn("docs/link.md", result["reason"])

    def test_a_free_file_replaced_by_a_symlink_is_full(self):
        self.git("checkout", "-q", "--detach", self.base)
        (self.path / "docs/SPEC.md").unlink()
        (self.path / "docs/SPEC.md").symlink_to("../crates/pbps-model/src/lib.rs")
        self.assertEqual(self.verdict(self.commit("type change"))["verdict"], "full")

    def test_an_edit_to_a_symlink_that_already_existed_is_full(self):
        (self.path / "docs/link.md").symlink_to("SPEC.md")
        base = self.commit("add link")
        self.git("checkout", "-q", "--detach", base)
        (self.path / "docs/link.md").unlink()
        (self.path / "docs/link.md").symlink_to("../README.md")
        self.assertEqual(self.verdict(self.commit("retarget"), base=base)["verdict"], "full")

    def test_a_symlink_replaced_by_a_regular_file_is_full(self):
        (self.path / "docs/link.md").symlink_to("SPEC.md")
        base = self.commit("add link")
        self.git("checkout", "-q", "--detach", base)
        (self.path / "docs/link.md").unlink()
        self.write("docs/link.md", "plain\n")
        self.assertEqual(self.verdict(self.commit("untype"), base=base)["verdict"], "full")

    def test_a_submodule_pointer_is_full(self):
        self.git("checkout", "-q", "--detach", self.base)
        self.git("update-index", "--add", "--cacheinfo", f"160000,{self.base},docs/vendored")
        result = self.verdict(self.commit_index("submodule"))
        self.assertEqual(result["verdict"], "full")
        self.assertIn("docs/vendored", result["reason"])

    def test_an_ordinary_executable_bit_change_is_not_indirect(self):
        self.git("checkout", "-q", "--detach", self.base)
        self.git("update-index", "--chmod=+x", "scripts/pr-size.py")
        self.assertEqual(self.verdict(self.commit_index("chmod"))["verdict"], "non-live")

    def test_a_head_equal_to_its_base_is_full(self):
        self.assertEqual(self.verdict(self.base)["verdict"], "full")


class Command(Repo):
    def run_main(self, *args):
        out = io.StringIO()
        with contextlib.redirect_stdout(out):
            code = lgs.main(["--repo", str(self.path), *args])
        return code, out.getvalue()

    def test_non_live_prints_one_json_line_and_exits_zero(self):
        head = self.change({"scripts/pr-size.py": "# changed\n"})
        code, out = self.run_main("--base", self.base, "--head", head)
        self.assertEqual(code, 0)
        self.assertEqual(out.count("\n"), 1)
        result = json.loads(out)
        self.assertEqual(result["verdict"], "non-live")
        self.assertEqual({"base", "head", "merge_base", "reason", "verdict", "classes"}, set(result))

    def test_full_exits_non_zero_and_says_why(self):
        head = self.change({"crates/pbps-model/src/lib.rs": "// edited\n"})
        code, out = self.run_main("--base", self.base, "--head", head)
        self.assertEqual(code, 1)
        self.assertEqual(json.loads(out)["verdict"], "full")

    def test_an_unreadable_ref_exits_non_zero(self):
        self.assertEqual(self.run_main("--base", "no-such-ref")[0], 1)

    def test_the_default_base_is_origin_master(self):
        self.assertEqual(json.loads(self.run_main("--head", self.base)[1])["base"], "origin/master")

    def test_a_usage_error_exits_two_and_prints_no_verdict(self):
        result = subprocess.run([sys.executable, str(SCRIPT), "--no-such-flag"], capture_output=True, text=True)
        self.assertEqual(result.returncode, 2)
        self.assertEqual(result.stdout, "")

    def test_the_script_runs_as_a_program(self):
        head = self.change({"docs/SPEC.md": "x\n"})
        result = subprocess.run([sys.executable, str(SCRIPT), "--repo", str(self.path), "--base", self.base,
                                 "--head", head], capture_output=True, text=True)
        self.assertEqual((result.returncode, json.loads(result.stdout)["verdict"]), (0, "non-live"))


def tracked(*pathspecs):
    out = subprocess.run(["git", "ls-files", "-z", "--", *pathspecs], cwd=ROOT, capture_output=True, check=True).stdout
    return [name.decode() for name in out.split(b"\0") if name]


ROOT_ENTRIES = {name.split("/")[0] for name in tracked()}


def rust_reaches(name, text):
    """The repository files a Rust source names outside `crates/`, as (kind, path).

    `include` is compiled in (`include_str!`, `include_bytes!`, `include!`,
    `#[path]`), resolved from the including file. `path` and `join` are read by
    a test at run time: a `../` literal resolved from the crate root, or a
    `.join("schemas")` step. `doc` is a `docs/*.md` literal that is not an
    include, and `workflow` is any `.github/` literal. A path built from
    variables is invisible here; the reads were found by hand (DEC-2859.1)."""
    code = "\n".join(line for line in text.splitlines() if not line.lstrip().startswith("//"))
    crate = "/".join(name.split("/")[:2])

    def resolve(base, relative):
        target = posixpath.normpath(posixpath.join(base, relative.lstrip("/")))
        if target.startswith(("..", "crates/")) or target.split("/")[0] not in ROOT_ENTRIES:
            return None  # inside crates, above the repository, or a path of a project the test builds
        return target

    found = []
    for m in re.finditer(r'(?:include_str!|include_bytes!|include!|#\[path = )\(?\s*"([^"]+)"', code):
        found.append(("include", resolve(posixpath.dirname(name), m[1])))
    for m in re.finditer(r'"(/?(?:\.\./)+[^"\s]*)"', code):
        found.append(("path", resolve(crate, m[1])))
    found += [("join", m[1] + "/x") for m in re.finditer(r'\.join\("(schemas|scripts)"\)', code)]
    found += [("workflow", m[1]) for m in re.finditer(r'"[^"\n]*?(\.github/[\w./-]*)"', code)]
    compiled = {path for kind, path in found if kind == "include"}
    found += [("doc", m[0]) for m in re.finditer(r"docs/[\w./-]+\.md", code) if m[0] not in compiled]
    return sorted({(("doc" if kind == "path" and path.startswith("docs/") else kind), path)
                   for kind, path in found if path})


class Drift(unittest.TestCase):
    """Measured against this worktree: the rule must learn what the runners learn."""

    def test_live_runners_name_only_scripts_the_rule_keeps_on_the_full_gate(self):
        files = tracked("scripts")
        seen, queue, named = set(), ["scripts/live-tests.sh", "scripts/live-tests-pg.sh"], set()
        while queue:
            current = queue.pop()
            if current in seen:
                continue
            seen.add(current)
            text = (ROOT / current).read_text(encoding="utf-8")
            for path in files:
                rest = path[len("scripts/"):]
                top = rest.split("/")[0]
                if "/" in rest:
                    hit = re.search(rf"(?<![\w.-]){re.escape(top)}/", text)
                else:
                    stem = re.escape(rest.removesuffix(".py"))
                    hit = re.search(rf"(?<![\w.-]){re.escape(rest)}(?![\w-])", text) or (
                        rest.endswith(".py") and re.search(rf"^\s*(import|from) {stem}\b", text, re.M))
                if not hit or path == current:
                    continue
                free, label = lgs.classify(path)
                if free:
                    self.assertIn(rest, FREED_BUT_NAMED, f"{current} names {path}, which the rule frees: add it to KEEP_FULL")
                    named.add(rest)
                elif path.endswith((".py", ".sh")):
                    queue.append(path)
        # An exemption nothing names any more is a stale claim.
        self.assertEqual(named, set(FREED_BUT_NAMED))
        self.assertTrue({"scripts/live-shards.py", "scripts/gate_rerun.py", "scripts/known_flakes.py",
                         "scripts/install-pg-partman.sh", "scripts/check-live-fixture.py"} <= seen)

    def test_rust_tests_reach_outside_crates_only_where_the_rule_knows(self):
        found = set()
        for name in tracked("crates/*.rs"):
            text = (ROOT / name).read_text(encoding="utf-8", errors="replace")
            if not re.search(r"docs/|scripts/|schemas|\.github|\.\./", text):
                continue
            for kind, path in rust_reaches(name, text):
                with self.subTest(file=name, kind=kind, path=path):
                    if kind == "doc":
                        found.add(path)
                    elif kind == "workflow":
                        self.fail(f"{name} reads a workflow, which a live run must not")
                    else:
                        self.assertFalse(lgs.classify(path)[0], f"{name} reads {path}, which the rule frees")
        self.assertEqual(found, KNOWN_DOC_READS, "a Rust test reads a doc the rule has not measured")

    def test_the_rust_scan_sees_each_shape_of_a_path_read(self):
        support = "crates/pbps-cli/tests/support/x.rs"
        shapes = [
            (support, 'include_str!("../../../../scripts/compatibility-matrix.json")',
             [("include", "scripts/compatibility-matrix.json")]),
            (support, 'include_str!(\n "../../../../docs/measurements/m.md")', [("include", "docs/measurements/m.md")]),
            ("crates/pbps-ui/src/lib.rs", 'include_str!("../../../Cargo.toml")', [("include", "Cargo.toml")]),
            ("crates/pbps-pg/tests/a.rs", 'Path::new(m).join("../../schemas/envelope.schema.json")',
             [("path", "schemas/envelope.schema.json")]),
            ("crates/pbps-pg/tests/a.rs", 'include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../scripts/x.json"))',
             [("path", "scripts/x.json")]),
            (support, 'p.join("..").join("schemas").join("envelope.schema.json")', [("join", "schemas/x")]),
            (support, 'read_to_string(repo("docs/STATUS.md"))', [("doc", "docs/STATUS.md")]),
            ("crates/pbps-model/tests/a.rs", 'join("../../docs/ORDERING.md")', [("doc", "docs/ORDERING.md")]),
            (support, 'include_str!("../../../../.github/workflows/ci.yml")',
             [("include", ".github/workflows/ci.yml"), ("workflow", ".github/workflows/ci.yml")]),
        ]
        for name, code, expected in shapes:
            with self.subTest(code=code):
                self.assertEqual(rust_reaches(name, code), expected)
        for code in ('// see docs/SPEC.md and ../scripts/x.py\nlet a = 1;', '#[path = "../../../pbps-diff/tests/x.rs"]',
                     'include!(concat!(env!("CARGO_MANIFEST_DIR"), "/../pbps-dialect/tests/x.rs"))',
                     'include_str!("../fixtures/a.json")', 'let t = "docs";', 'let dir = root.join("tests");'):
            with self.subTest(code=code):
                self.assertEqual(rust_reaches(support, code), [])

    def test_no_live_runner_or_helper_reads_the_workflow_directory(self):
        for name in ("live-tests.sh", "live-tests-pg.sh", "live-test-status.sh", "live-transport.py",
                     "check-live-fixture.py", "resolver-mssql-fixtures.sh", "install-pg-partman.sh"):
            self.assertNotIn(".github", (ROOT / "scripts" / name).read_text(encoding="utf-8"), name)


if __name__ == "__main__":
    unittest.main()
