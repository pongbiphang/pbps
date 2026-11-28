#!/usr/bin/env python3
"""Tests for scripts/check-engine-lists.py. Run: python3 scripts/check_engine_lists_test.py"""

import importlib.util
import json
import tempfile
import unittest
from pathlib import Path

spec = importlib.util.spec_from_file_location(
    "check_engine_lists", Path(__file__).with_name("check-engine-lists.py")
)
cel = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cel)

REPO_ROOT = Path(__file__).resolve().parents[1]

BASE_CONFIG = """
pub enum DialectName {
    Mssql,
    Postgres,
    Sqlite,
}

impl DialectName {
    pub const ALL: &'static [Self] = &[Self::Mssql, Self::Postgres, Self::Sqlite];

    pub const fn as_str(self) -> &'static str {
        match self {
            DialectName::Mssql => "mssql",
            DialectName::Postgres => "postgres",
            DialectName::Sqlite => "sqlite",
        }
    }
}
"""

BASE_MARKER = {
    "version": 1,
    "engines": {
        "mssql": {
            "mode": "server",
            "live_job": "live",
            "live_runner": "scripts/live-tests.sh",
            "shard_suite": "mssql",
            "ignored_owner": "mssql",
            "doc_label": "SQL Server",
            "crate": "pbps-mssql",
        },
        "postgres": {
            "mode": "server",
            "live_job": "live-pg",
            "live_runner": "scripts/live-tests-pg.sh",
            "shard_suite": "pg",
            "ignored_owner": "pg",
            "doc_label": "PostgreSQL",
            "crate": "pbps-pg",
        },
        "sqlite": {
            "mode": "in-process",
            "reason": "Runs in process with no container and no ignored tests.",
            "doc_label": "SQLite",
            "crate": "pbps-sqlite",
        },
    },
}

BASE_WORKFLOW = """name: CI
jobs:
  lint:
    runs-on: ubuntu-latest
  live:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test -p pbps-mssql --test live -- --ignored
  live-pg:
    runs-on: ubuntu-latest
    steps:
      - run: cargo test -p pbps-pg --test live -- --ignored
  compatibility:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        cell: [mssql2025, pg18]
  gate:
    needs: [lint, live, live-pg, compatibility]
"""

BASE_COMPAT = {
    "mssql2025": {"engine": "mssql", "image": "mssql@sha256:1"},
    "pg18": {"engine": "postgres", "image": "pg@sha256:1"},
}

BASE_SHARDS = """
RUNNERS = {"mssql": "live-tests.sh", "pg": "live-tests-pg.sh"}
FIXTURES = {
    "mssql": [("A", "B", "mssql", 26330)],
    "pg": [("C", "D", "pg", 27320)],
}
"""

BASE_OWNERS = {
    "version": 1,
    "owners": {
        "mssql": {"platform": "linux", "job": "live"},
        "pg": {"platform": "linux", "job": "live-pg"},
    },
}

BASE_RULES = {
    "version": 1,
    "rules": [
        {"target": ["pbps-mssql", "test", "live"], "prefix": "", "owner": "mssql"},
        {"target": ["pbps-pg", "test", "live"], "prefix": "", "owner": "pg"},
    ],
}

BASE_SPEC = """# Spec

### 11.3 Principal dependencies

| Purpose | Crate | Notes |
|---|---|---|
| SQL Server | `tiberius` | |
| PostgreSQL | `tokio-postgres` | |
| SQLite | `rusqlite` | |
"""

BASE_ARCH = """# Architecture

## Architectural boundaries

```text
pbps-mssql     SQL Server
pbps-pg        PostgreSQL
pbps-sqlite    SQLite
```
"""


def write_tree(root, overrides=None, remove=()):
    files = {
        cel.CONFIG_SOURCE: BASE_CONFIG,
        cel.MARKER_FILE: json.dumps(BASE_MARKER, indent=2),
        cel.WORKFLOW_FILE: BASE_WORKFLOW,
        cel.COMPAT_MATRIX_FILE: json.dumps(BASE_COMPAT, indent=2),
        cel.SHARDS_FILE: BASE_SHARDS,
        cel.OWNERS_FILE: json.dumps(BASE_OWNERS, indent=2),
        cel.RULES_FILE: json.dumps(BASE_RULES, indent=2),
        cel.SPEC_FILE: BASE_SPEC,
        cel.ARCH_FILE: BASE_ARCH,
        "scripts/live-tests.sh": "#!/usr/bin/env bash\nexec python3 scripts/live-shards.py shard mssql\n",
        "scripts/live-tests-pg.sh": "#!/usr/bin/env bash\nexec python3 scripts/live-shards.py shard pg\n",
    }
    if overrides:
        files.update(overrides)
    for rel in remove:
        files.pop(rel, None)
    for rel, content in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")


class EngineListsTest(unittest.TestCase):
    def test_real_repository_passes(self):
        self.assertEqual(cel.check(REPO_ROOT), [])

    def test_synthetic_clean_tree_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_tree(Path(tmp))
            self.assertEqual(cel.check(Path(tmp)), [])

    def test_fourth_engine_added_to_dialect_name_without_marker_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = BASE_CONFIG.replace(
                "    Sqlite,\n}",
                "    Sqlite,\n    Mysql,\n}",
            ).replace(
                "Self::Sqlite];",
                "Self::Sqlite, Self::Mysql];",
            ).replace(
                'DialectName::Sqlite => "sqlite",',
                'DialectName::Sqlite => "sqlite",\n            DialectName::Mysql => "mysql",',
            )
            write_tree(Path(tmp), {cel.CONFIG_SOURCE: config})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("mysql" in e and cel.MARKER_FILE in e for e in errors),
                errors,
            )

    def test_variant_missing_from_dialect_name_all_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            config = BASE_CONFIG.replace(", Self::Sqlite];", "];")
            write_tree(Path(tmp), {cel.CONFIG_SOURCE: config})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("Sqlite" in e and "DialectName::ALL" in e for e in errors),
                errors,
            )

    def test_stale_engine_in_marker_file_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = json.loads(json.dumps(BASE_MARKER))
            marker["engines"]["oracle"] = {
                "mode": "in-process",
                "reason": "stale",
                "doc_label": "Oracle",
                "crate": "pbps-oracle",
            }
            write_tree(Path(tmp), {cel.MARKER_FILE: json.dumps(marker)})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("oracle" in e and cel.MARKER_FILE in e for e in errors),
                errors,
            )

    def test_an_entry_that_is_not_an_object_is_not_a_registration(self):
        # `null` is a key that is present: it must not pass as "registered".
        for value in (None, "in-process", [], 1):
            with tempfile.TemporaryDirectory() as tmp:
                marker = json.loads(json.dumps(BASE_MARKER))
                marker["engines"]["sqlite"] = value
                write_tree(Path(tmp), {cel.MARKER_FILE: json.dumps(marker)})
                self.assertEqual(
                    cel.check(Path(tmp)),
                    ["engine `sqlite`: scripts/engine-ci.json: entry must be a JSON object"],
                    repr(value),
                )

    def test_in_process_marker_without_reason_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            marker = json.loads(json.dumps(BASE_MARKER))
            marker["engines"]["sqlite"]["reason"] = "   "
            write_tree(Path(tmp), {cel.MARKER_FILE: json.dumps(marker)})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("sqlite" in e and "reason" in e for e in errors),
                errors,
            )

    def test_missing_ci_live_job_and_gate_need_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            workflow = BASE_WORKFLOW.replace(
                "  live-pg:\n    runs-on: ubuntu-latest\n    steps:\n"
                "      - run: cargo test -p pbps-pg --test live -- --ignored\n",
                "",
            )
            self.assertNotEqual(workflow, BASE_WORKFLOW)
            write_tree(Path(tmp), {cel.WORKFLOW_FILE: workflow})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.WORKFLOW_FILE in e and "live-pg" in e for e in errors),
                errors,
            )

    def test_a_live_job_that_no_longer_runs_the_engines_tests_fails(self):
        step = "      - run: cargo test -p pbps-pg --test live -- --ignored\n"
        for replacement in (
            "      - run: echo skipped\n",
            # A comment that names the command is not a step.
            "      # - run: cargo test -p pbps-pg --test live -- --ignored\n",
            # Another engine's tests do not cover this one.
            "      - run: cargo test -p pbps-mssql --test live -- --ignored\n",
            # Without `--ignored` the live tests are compiled and skipped.
            "      - run: cargo test -p pbps-pg --test live\n",
        ):
            with tempfile.TemporaryDirectory() as tmp:
                workflow = BASE_WORKFLOW.replace(step, replacement)
                self.assertNotEqual(workflow, BASE_WORKFLOW)
                write_tree(Path(tmp), {cel.WORKFLOW_FILE: workflow})
                errors = cel.check(Path(tmp))
                self.assertEqual(
                    [e for e in errors if "runs no" in e],
                    [
                        "engine `postgres`: .github/workflows/ci.yml: live job `live-pg` "
                        "runs no `cargo test -p pbps-pg ... --ignored` step"
                    ],
                    replacement,
                )

    def test_missing_compatibility_cell_in_ci_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            workflow = BASE_WORKFLOW.replace("cell: [mssql2025, pg18]", "cell: [mssql2025]")
            write_tree(Path(tmp), {cel.WORKFLOW_FILE: workflow})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.WORKFLOW_FILE in e and "pg18" in e for e in errors),
                errors,
            )

    def test_missing_compatibility_matrix_engine_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            compat = {"mssql2025": BASE_COMPAT["mssql2025"]}
            write_tree(Path(tmp), {cel.COMPAT_MATRIX_FILE: json.dumps(compat)})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.COMPAT_MATRIX_FILE in e for e in errors),
                errors,
            )

    def test_missing_live_runner_or_shard_entry_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            write_tree(Path(tmp), remove=("scripts/live-tests-pg.sh",))
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("scripts/live-tests-pg.sh" in e and "cannot be read" in e for e in errors),
                errors,
            )
        with tempfile.TemporaryDirectory() as tmp:
            shards = BASE_SHARDS.replace(', "pg": "live-tests-pg.sh"', "")
            write_tree(Path(tmp), {cel.SHARDS_FILE: shards})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.SHARDS_FILE in e and "RUNNERS" in e for e in errors),
                errors,
            )

    def test_missing_ignored_test_owner_or_rule_fails(self):
        with tempfile.TemporaryDirectory() as tmp:
            owners = json.loads(json.dumps(BASE_OWNERS))
            del owners["owners"]["pg"]
            write_tree(Path(tmp), {cel.OWNERS_FILE: json.dumps(owners)})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.OWNERS_FILE in e for e in errors),
                errors,
            )
        with tempfile.TemporaryDirectory() as tmp:
            rules = {"version": 1, "rules": [BASE_RULES["rules"][0]]}
            write_tree(Path(tmp), {cel.RULES_FILE: json.dumps(rules)})
            errors = cel.check(Path(tmp))
            self.assertTrue(
                any("postgres" in e and cel.RULES_FILE in e for e in errors),
                errors,
            )

    def test_missing_doc_entries_in_spec_and_architecture_fail(self):
        with tempfile.TemporaryDirectory() as tmp:
            spec = BASE_SPEC.replace("| SQLite | `rusqlite` | |\n", "")
            arch = BASE_ARCH.replace("pbps-sqlite    SQLite\n", "")
            write_tree(Path(tmp), {cel.SPEC_FILE: spec, cel.ARCH_FILE: arch})
            errors = cel.check(Path(tmp))
            self.assertTrue(any("sqlite" in e and cel.SPEC_FILE in e for e in errors), errors)
            self.assertTrue(any("sqlite" in e and cel.ARCH_FILE in e for e in errors), errors)

    def test_unreadable_or_missing_files_fail_loudly(self):
        for missing in (
            cel.CONFIG_SOURCE,
            cel.MARKER_FILE,
            cel.WORKFLOW_FILE,
            cel.COMPAT_MATRIX_FILE,
            cel.SHARDS_FILE,
            cel.OWNERS_FILE,
            cel.RULES_FILE,
            cel.SPEC_FILE,
            cel.ARCH_FILE,
        ):
            with self.subTest(missing=missing):
                with tempfile.TemporaryDirectory() as tmp:
                    write_tree(Path(tmp), remove=(missing,))
                    errors = cel.check(Path(tmp))
                    self.assertTrue(
                        any(missing in e and "cannot be read" in e for e in errors),
                        errors,
                    )

    def test_rust_comments_do_not_confuse_dialect_parser(self):
        config = """
        // pub enum DialectName { Fake }
        /* impl DialectName { pub const ALL: &'static [Self] = &[]; } */
        pub enum DialectName {
            /// SQL Server
            Mssql,
            Postgres, // PostgreSQL
        }
        impl DialectName {
            pub const ALL: &'static [Self] = &[Self::Mssql, Self::Postgres];
            pub const fn as_str(self) -> &'static str {
                match self {
                    // Self::Fake => "fake",
                    DialectName::Mssql => "mssql",
                    DialectName::Postgres => "postgres",
                }
            }
        }
        """
        names, errors = cel.parse_dialect_names(config)
        self.assertEqual(errors, [])
        self.assertEqual(names, ["mssql", "postgres"])


if __name__ == "__main__":
    unittest.main()
