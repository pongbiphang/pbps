#!/usr/bin/env python3
"""Verify native target identity using disposable engine and TLS fixtures.

Default mode confines the inspector to the owned target's PID/network
namespaces. --native-host exercises the public Docker factory on a disposable
native Linux runner; it requires root and a direct native Docker daemon.
"""

import argparse
from fixture_diagnostics import report
import json
import os
from pathlib import Path
import signal
import shutil
import subprocess
import sys
import tempfile
import time
import uuid


PG_IMAGES = {
    16: "postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8",
    18: "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
}
IMAGES = {
    "pg": PG_IMAGES[18],
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
PASSWORD = "Pbps!NativeFixture12345"
TARGET_TEST = "resolver::native::target::tests::native_aliases_share_one_instance_and_backend_children_cannot_claim_another"
FACTORY_TEST = "resolver::docker::session::native_factory_tests::native_factory_qualifies_before_bootstrap_and_rejects_rebound_target_connections"
RECIPE_TEST = "resolver::docker::session::pg_recipe_tests::the_public_factory_runs_the_pinned_engine_on_bounded_private_storage"
ANALYSIS_TEST = "resolver::server::container_tests::the_owned_container_resolves_the_overload_pair_on_its_qualified_connections"
INVALIDATION_TEST = "resolver::server::container_tests::a_changed_owned_runtime_or_target_ends_container_analysis_permanently"
CANCELLATION_TEST = "resolver::server::container_tests::cancelled_container_analysis_removes_or_names_every_owned_resource"
DEADLINE_TEST = "resolver::server::container_tests::delayed_container_analysis_expires_at_its_first_owner_bound"
ADMIN_RECOVERY_TEST = "resolver::server::container_tests::relay_recovery::admin_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners"
SCRATCH_RECOVERY_TEST = "resolver::server::container_tests::relay_recovery::scratch_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners"
JANITOR_RECOVERY_TEST = "resolver::server::container_tests::relay_recovery::janitor_launch_recovery_distinguishes_absence_from_uncertainty_without_losing_other_owners"
DAEMON_TEST = "resolver::docker::tests::direct_native_daemon_is_accepted_but_a_root_owned_proxy_is_not"
SQL_RECOVERY_CLOSE_TEST = "resolver::server::container_tests::sql_recovery::confirmed_workload_removal_finishes_only_its_existing_sql_recovery"
SQL_RECOVERY_DISCARD_TEST = "resolver::server::container_tests::sql_recovery::interrupted_open_discard_finishes_sql_recovery_only_after_workload_removal"
SQL_RECOVERY_CANCEL_TEST = "resolver::server::container_tests::sql_recovery::cancelled_cleanup_keeps_uncertain_owned_sql_names_and_remains_terminal"
PG18_GENERATION_TEST = "resolver::server::qualified_evidence_tests::the_pg18_producer_projects_the_replaced_generated_attrdef_and_preserves_column_identity"
PG16_GENERATION_TEST = "resolver::server::qualified_evidence_tests::the_pg16_producer_adds_stored_generation_and_retains_existing_generated_bindings"
# Major-specific cases stay separate from the existing producer-only suite.
GENERATION_TESTS = [PG18_GENERATION_TEST, PG16_GENERATION_TEST]
PRODUCER_TESTS = [
    "resolver::server::qualified_evidence_tests::the_container_producer_seals_the_overload_and_default_from_one_fresh_read",
    "resolver::server::qualified_evidence_tests::only_recorded_table_and_index_roots_own_their_catalog_columns",
    "resolver::server::qualified_evidence_tests::a_same_named_view_cannot_own_the_declared_table_uid",
    "resolver::server::qualified_evidence_tests::a_same_named_index_on_another_table_cannot_own_the_declared_index",
    "resolver::server::qualified_evidence_tests::the_connected_producer_refuses_a_missing_environment_key_before_sealing",
    "resolver::server::qualified_evidence_tests::a_rebuilt_view_with_old_target_grants_closes_on_the_actual_catalog",
    "resolver::server::qualified_evidence_tests::an_empty_target_producer_orders_table_routines_and_expressions_before_sealing",
    "resolver::server::qualified_evidence_tests::a_replaced_routine_rebuilds_cross_kind_dependents_in_the_final_plan",
    "resolver::server::qualified_evidence_tests::recorded_table_and_column_uids_survive_rename_with_dependent_rebuilds",
    "resolver::server::qualified_evidence_tests::recorded_rename_of_a_table_owned_by_another_role_closes_on_the_actual_catalog",
    "resolver::server::qualified_evidence_tests::rebuilt_routine_with_explicit_public_execution_matches_the_post_ddl_acl",
    "resolver::server::qualified_evidence_tests::rebuilt_routine_replays_declared_role_grants",
    "resolver::server::qualified_evidence_tests::an_unaffected_routine_keeps_its_grant_option_through_connected_evidence",
    "resolver::server::qualified_evidence_tests::newly_created_routines_under_target_default_privileges_close_on_the_actual_catalog",
    "resolver::server::qualified_evidence_tests::a_new_routine_created_by_an_ordinary_deployer_closes_on_the_actual_catalog",
    "resolver::server::qualified_evidence_tests::an_explicit_extra_schema_changes_only_the_unqualified_lookup_in_the_final_plan",
    "resolver::server::qualified_evidence_tests::a_prequalified_matching_scope_seals_the_same_ordinary_plan",
    "resolver::server::qualified_evidence_tests::a_prequalified_scope_rejects_reordered_write_path_extras_before_compilation",
    "resolver::server::qualified_evidence_tests::a_prequalified_scope_rejects_changed_preliminary_grants_before_compilation",
    "resolver::server::qualified_evidence_tests::recorded_renames_and_reused_old_spellings_keep_distinct_owned_inventories",
    "resolver::server::qualified_evidence_tests::recorded_renames_with_type_and_nullability_edits_match_actual_child_catalog",
    "resolver::server::qualified_evidence_tests::persisted_authorization_is_stable_across_processes_only_with_the_same_environment_key",
    "resolver::server::qualified_evidence_tests::adding_named_key_and_check_keeps_existing_table_owner_and_grants",
    "resolver::server::qualified_evidence_tests::adding_check_alone_keeps_existing_table_owner_and_grants",
    "resolver::server::qualified_evidence_tests::granting_on_an_existing_table_keeps_old_acl_and_adds_the_declared_role",
    "resolver::server::qualified_evidence_tests::unnamed_primary_key_on_existing_table_owns_only_its_exact_constraint_and_index",
    "resolver::server::qualified_evidence_tests::unnamed_primary_key_on_created_table_owns_only_its_exact_constraint_and_index",
    "resolver::server::qualified_evidence_tests::adding_index_keeps_existing_table_metadata_and_sets_the_engine_index_flag",
    "resolver::server::qualified_evidence_tests::direct_schema_option_beats_inherited_owner_for_the_typed_grant",
    "resolver::server::qualified_evidence_tests::combined_schema_grant_uses_one_inherited_grantor_for_its_whole_mask",
]
NATIVE_TESTS = [DAEMON_TEST, TARGET_TEST, FACTORY_TEST, RECIPE_TEST, ANALYSIS_TEST,
                INVALIDATION_TEST, CANCELLATION_TEST, DEADLINE_TEST,
                ADMIN_RECOVERY_TEST, SCRATCH_RECOVERY_TEST, JANITOR_RECOVERY_TEST,
                SQL_RECOVERY_CLOSE_TEST, SQL_RECOVERY_DISCARD_TEST,
                SQL_RECOVERY_CANCEL_TEST] + PRODUCER_TESTS + GENERATION_TESTS
QUIET = {"stdout": subprocess.DEVNULL, "stderr": subprocess.DEVNULL}
DOCKER_SOCKET = "/var/run/docker.sock"


def fixture_image(engine, pg_major):
    return PG_IMAGES[pg_major] if engine == "pg" else IMAGES[engine]


def run(*args, **kwargs):
    if args[0] == "docker":
        args = ("docker", "--host", "unix://" + DOCKER_SOCKET, *args[1:])
    kwargs.setdefault("check", True)
    return subprocess.run(args, text=True, **kwargs)


def interrupted(signum, _frame):
    raise SystemExit(128 + signum)


def native_tests(binary, env, producer_only=False, generation_only=False):
    if producer_only and generation_only:
        raise RuntimeError("producer-only and generation-only are mutually exclusive")
    if generation_only:
        if env.get("PBPS_NATIVE_DRIVER") != "pg":
            raise RuntimeError("generation-only requires PBPS_NATIVE_DRIVER=pg")
        if env.get("PBPS_NATIVE_PG_MAJOR") not in ("16", "18"):
            raise RuntimeError("generation-only requires PBPS_NATIVE_PG_MAJOR=16 or 18")
    # The daemon/proxy case needs the same disposable root host as the factory;
    # the ordinary library run only compiles it and leaves it ignored.
    for test in GENERATION_TESTS if generation_only else PRODUCER_TESTS if producer_only else NATIVE_TESTS:
        if test in (RECIPE_TEST, ANALYSIS_TEST, INVALIDATION_TEST, CANCELLATION_TEST,
                    DEADLINE_TEST, ADMIN_RECOVERY_TEST, SCRATCH_RECOVERY_TEST,
                    JANITOR_RECOVERY_TEST, SQL_RECOVERY_CLOSE_TEST,
                    SQL_RECOVERY_DISCARD_TEST, SQL_RECOVERY_CANCEL_TEST,
                    *PRODUCER_TESTS) and env.get("PBPS_NATIVE_DRIVER") != "pg":
            continue
        if test in GENERATION_TESTS:
            major = "18" if test == PG18_GENERATION_TEST else "16"
            # Each case verifies the actual major; selection alone is not proof.
            if env.get("PBPS_NATIVE_DRIVER") != "pg" or env.get("PBPS_NATIVE_PG_MAJOR") != major:
                continue
        result = run(binary, "--ignored", "--exact", test, "--nocapture",
                     env=dict(os.environ, **env), stdout=subprocess.PIPE,
                     stderr=subprocess.STDOUT, check=False)
        print(result.stdout, end="", flush=True)
        if result.returncode or "test result: ok. 1 passed" not in result.stdout:
            raise RuntimeError("native fixture did not run exactly one passing test: " + test)


def test_binary():
    built = run("cargo", "test", "--profile", "live-test", "-p", "pbps-cli", "--lib", "--no-run",
                "--message-format=json", stdout=subprocess.PIPE)
    artifacts = [json.loads(line) for line in built.stdout.splitlines()]
    return next(item["executable"] for item in artifacts
                if item.get("executable") and item.get("target", {}).get("name") == "pbps_cli")


def certificates(root):
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "2",
        "-subj", "/CN=pbps native test CA", "-keyout", str(root / "ca.key"),
        "-out", str(root / "ca.pem"), **QUIET)
    run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=localhost",
        "-keyout", str(root / "peer.key"), "-out", str(root / "peer.csr"), **QUIET)
    (root / "extensions").write_text(
        "subjectAltName=DNS:localhost,IP:127.0.0.1\nbasicConstraints=CA:FALSE\n"
        "keyUsage=digitalSignature,keyEncipherment\nextendedKeyUsage=serverAuth\n")
    run("openssl", "x509", "-req", "-in", str(root / "peer.csr"), "-CA", str(root / "ca.pem"),
        "-CAkey", str(root / "ca.key"), "-CAcreateserial", "-days", "2", "-extfile",
        str(root / "extensions"), "-out", str(root / "peer.pem"), **QUIET)
    (root / "empty-ca").mkdir()


def fixture(args, binary, root, owned):
    certificates(root)
    engine = args.engine
    image = fixture_image(engine, args.pg_major)
    name = "pbps-native-target-" + uuid.uuid4().hex
    if engine == "pg":
        # The owned resolver launches with C.UTF-8; keep target session-wide
        # locale settings comparable while preserving actual engine checks.
        environment = ["-e", f"POSTGRES_PASSWORD={PASSWORD}", "-e", "LANG=C.UTF-8"]
        boot = ("chown postgres:postgres /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec docker-entrypoint.sh postgres -c listen_addresses=127.0.0.1 "
                "-c ssl=on -c ssl_cert_file=/tmp/peer.pem -c ssl_key_file=/tmp/peer.key")
        probe = ["pg_isready", "-h", "127.0.0.1", "-U", "postgres"]
    else:
        environment = ["-e", "ACCEPT_EULA=Y", "-e", f"MSSQL_SA_PASSWORD={PASSWORD}",
                       "-e", "MSSQL_MEMORY_LIMIT_MB=1024"]
        boot = ("chown mssql:root /tmp/peer.key; chmod 600 /tmp/peer.key; "
                "exec su -s /bin/bash mssql -c /opt/mssql/bin/sqlservr")
        probe = ["/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1", "-U", "sa",
                 "-P", PASSWORD, "-Q", "SELECT 1"]
        (root / "mssql.conf").write_text(
            "[network]\nipaddress=127.0.0.1\ntlscert=/tmp/peer.pem\n"
            "tlskey=/tmp/peer.key\nforceencryption=0\n")
    # Record the exact generated name before create so interrupted replies
    # cannot lose its cleanup scope. No shared test container is modified.
    owned.append(name)
    pid_scope = ["--pid=host"] if args.native_host else []
    target = run("docker", "create", "--name", name, "--pull", "never", "--network",
                 "host" if args.native_host else "none", "--user", "0", "--memory", "3g",
                 "--cpus", "2", "--pids-limit", "512", *pid_scope, *environment,
                 "--entrypoint", "/bin/bash", image, "-ec", boot,
                 stdout=subprocess.PIPE).stdout.strip()
    for leaf in ("peer.key", "peer.pem"):
        run("docker", "cp", str(root / leaf), target + ":/tmp/" + leaf, **QUIET)
    if engine == "mssql":
        run("docker", "cp", str(root / "mssql.conf"), target + ":/var/opt/mssql/mssql.conf", **QUIET)
    # `check=False` and reported: the wrapper's default would raise straight
    # past `main`'s cleanup, which removes the container, so an engine that
    # refused to start at all printed neither state nor log (#724).
    if run("docker", "start", target, check=False, **QUIET).returncode:
        report(run, target)
        raise RuntimeError("owned native TLS fixture did not start")
    for _ in range(60):
        if run("docker", "exec", target, *probe, check=False, **QUIET).returncode == 0:
            break
        time.sleep(1)
    else:
        # This said only that it failed, and the cleanup then removed the
        # container: an engine that crashed while starting and a fault in the
        # fixture read identically (#724).
        report(run, target)
        raise RuntimeError("owned native TLS fixture failed to start")
    trust = str(root / "ca.pem") if args.native_host else "/tmp/ca.pem"
    if engine == "pg":
        statements = ["CREATE DATABASE pbps_native_alias",
                      f"CREATE ROLE pbps_native_alt LOGIN PASSWORD '{PASSWORD}'",
                      "GRANT EXECUTE ON FUNCTION pg_catalog.pg_control_system() TO pbps_native_alt"]
        for statement in statements:
            run("docker", "exec", "-e", f"PGPASSWORD={PASSWORD}", target, "psql", "-h",
                "127.0.0.1", "-U", "postgres", "-v", "ON_ERROR_STOP=1", "-c", statement, **QUIET)
        primary = f"host=localhost port=5432 user=postgres password={PASSWORD} dbname=postgres sslmode=require"
        alias = f"host=127.0.0.1 port=5432 user=pbps_native_alt password={PASSWORD} dbname=pbps_native_alias sslmode=require"
        executable = "postgres"
    else:
        batches = [
            ("master", "CREATE DATABASE pbps_native_alias"),
            ("master", f"CREATE LOGIN pbps_native_alt WITH PASSWORD='{PASSWORD}', CHECK_POLICY=OFF; "
             "GRANT VIEW ANY DATABASE TO pbps_native_alt; CREATE USER pbps_native_alt FOR LOGIN pbps_native_alt"),
            ("pbps_native_alias", "CREATE USER pbps_native_alt FOR LOGIN pbps_native_alt"),
        ]
        for database, sql in batches:
            run("docker", "exec", target, "/opt/mssql-tools18/bin/sqlcmd", "-C", "-S", "127.0.0.1",
                "-U", "sa", "-P", PASSWORD, "-d", database, "-b", "-Q", sql, **QUIET)
        primary = f"Server=localhost,1433;User Id=sa;Password={PASSWORD};Database=master;Encrypt=true;TrustServerCertificateCA={trust}"
        alias = f"Server=127.0.0.1,1433;User Id=pbps_native_alt;Password={PASSWORD};Database=pbps_native_alias;Encrypt=true;TrustServerCertificateCA={trust}"
        executable = "sqlservr"
    ps = (["docker", "top", target, "-eo", "pid,ppid,comm"] if args.native_host
          else ["docker", "exec", target, "/bin/ps", "-eo", "pid,ppid,comm"])
    rows = [line.split() for line in run(*ps, stdout=subprocess.PIPE).stdout.splitlines()[1:]]
    processes = {row[0] for row in rows if row[2] == executable}
    roots = [row[0] for row in rows if row[0] in processes and row[1] not in processes]
    if len(roots) != 1:
        raise RuntimeError("ambiguous owned fixture service")
    env = dict(PBPS_NATIVE_DRIVER=engine, PBPS_NATIVE_SERVICE_PID=roots[0],
               PBPS_NATIVE_CONNECTION=primary, PBPS_NATIVE_ALIAS_CONNECTION=alias,
               SSL_CERT_FILE=trust,
               SSL_CERT_DIR=str(root / "empty-ca") if args.native_host else "/tmp/empty-ca")
    env["PATH"] = "/pbps-no-external-tools"
    env["PBPS_NATIVE_PROXY_CERT"] = str(root / "peer.pem") if args.native_host else "/tmp/proxy.pem"
    env["PBPS_NATIVE_PROXY_KEY"] = str(root / "peer.key") if args.native_host else "/tmp/proxy.key"
    if args.native_host:
        env.update(PBPS_NATIVE_FACTORY_FIXTURE="1", PBPS_RESOLVER_TEST_SOCKET=args.socket,
                   PBPS_RESOLVER_TEST_IMAGE=image)
        if engine == "pg":
            env["PBPS_NATIVE_PG_MAJOR"] = str(args.pg_major)
            env["PBPS_NATIVE_DOCKER"] = str(Path(shutil.which("docker")).resolve())
        native_tests(binary, env, args.producer_only, args.generation_only)
        return
    reader = "pbps-native-reader-" + uuid.uuid4().hex
    owned.append(reader)
    variables = [arg for key, value in env.items() for arg in ("-e", key + "=" + value)]
    run("docker", "create", "--name", reader, "--pull", "never", "--network", "container:" + target,
        "--pid", "container:" + target, "--cap-drop", "ALL", "--cap-add", "SYS_PTRACE", "--cap-add",
        "DAC_READ_SEARCH", "--cap-add", "KILL", "--security-opt", "no-new-privileges", "--memory",
        "768m", "--cpus", "1", "--pids-limit", "256", *variables, "--entrypoint", "/usr/bin/timeout",
        IMAGES["pg"], "--signal=KILL", "45s", "/pbps-cli-tests", "--ignored", "--exact",
        TARGET_TEST, "--nocapture", **QUIET)
    run("docker", "cp", binary, reader + ":/pbps-cli-tests", **QUIET)
    run("docker", "cp", str(root / "ca.pem"), reader + ":/tmp/ca.pem", **QUIET)
    run("docker", "cp", str(root / "peer.pem"), reader + ":/tmp/proxy.pem", **QUIET)
    run("docker", "cp", str(root / "peer.key"), reader + ":/tmp/proxy.key", **QUIET)
    result = run("docker", "start", "--attach", reader, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                 timeout=60, check=False)
    print(result.stdout, end="", flush=True)
    status = run("docker", "inspect", "--format", "{{.State.ExitCode}}", reader,
                 stdout=subprocess.PIPE).stdout.strip()
    if result.returncode or status != "0" or "test result: ok. 1 passed" not in result.stdout:
        raise RuntimeError("native target fixture failed")


def main():
    global DOCKER_SOCKET
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=IMAGES)
    parser.add_argument("--pg-major", type=int, choices=PG_IMAGES, default=18,
                        help="pinned PostgreSQL fixture image; default: 18")
    parser.add_argument("--native-host", action="store_true")
    focused = parser.add_mutually_exclusive_group()
    focused.add_argument("--producer-only", action="store_true",
                         help="run only the focused #1274 producer cases")
    focused.add_argument("--generation-only", action="store_true",
                         help="run only the connected generated-expression case for this PG major")
    parser.add_argument("--socket", default="/var/run/docker.sock")
    parser.add_argument("--test-binary", type=Path, help="prebuilt CLI library test executable")
    args = parser.parse_args()
    if args.engine != "pg" and args.pg_major != 18:
        parser.error("--pg-major applies only to pg")
    if args.producer_only and (not args.native_host or args.engine != "pg"):
        parser.error("--producer-only requires --native-host pg")
    if args.generation_only and (not args.native_host or args.engine != "pg"):
        parser.error("--generation-only requires --native-host pg")
    if sys.platform != "linux":
        parser.error("native process qualification requires Linux")
    if not Path(args.socket).is_absolute():
        parser.error("the Docker socket must be absolute")
    DOCKER_SOCKET = args.socket
    if args.native_host and os.geteuid() != 0:
        parser.error("--native-host requires root on an explicitly disposable native Linux runner")
    signal.signal(signal.SIGINT, interrupted)
    signal.signal(signal.SIGTERM, interrupted)
    binary = str(args.test_binary.resolve()) if args.test_binary else test_binary()
    owned = []
    try:
        with tempfile.TemporaryDirectory(prefix="pbps-native-pki-") as directory:
            fixture(args, binary, Path(directory), owned)
    finally:
        for resource in reversed(owned):
            run("docker", "rm", "--force", "--volumes", resource, stdout=subprocess.DEVNULL)


if __name__ == "__main__":
    main()
