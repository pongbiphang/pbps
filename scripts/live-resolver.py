#!/usr/bin/env python3
"""Exercise explicitly selected Docker resolver fixtures on Linux.

Each ignored test owns and removes its containers. Native kernel inspection
uses read-only proc metadata from the owned runtime; source-free transport
tests remain distinct from the complete public factory test.
"""

import argparse
from fixture_diagnostics import report, self_check
import json
import os
from pathlib import Path
import re
import subprocess
import sys


IMAGES = {
    "pg": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
TESTS = [
    "docker::tests::local_image_acquisition_observes_immutable_content_without_pulling",
    "docker::lifecycle::tests::fixed_launch_survives_bootstrap_and_explicit_close_removes_the_live_candidate",
    "docker::lifecycle::tests::the_root_deadline_removes_even_a_detached_descendant",
    "docker::lifecycle::tests::external_sentinels_are_unreachable_until_the_corresponding_boundary_is_removed",
    "docker::reserved::tests::a_reserved_runtime_has_no_database_until_its_fixed_bootstrap_gate_opens",
    "docker::session::tests::the_private_channel_compiles_declarations_and_control_loss_discards_the_run",
    "native::execution::tests::owned_root_guard_controls_are_measured_on_the_native_kernel",
    "docker::session::kernel_tests::private_channel_kernel_pairing_uses_only_owned_process_mounts",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=IMAGES)
    parser.add_argument("--socket", default="/var/run/docker.sock")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("these measured runtime profiles require Linux")
    socket = Path(args.socket)
    if not socket.is_absolute():
        parser.error("the selected Docker socket must be absolute")
    env = dict(os.environ, PBPS_RESOLVER_TEST_SOCKET=str(socket),
               PBPS_RESOLVER_TEST_DRIVER=args.engine,
               PBPS_RESOLVER_TEST_IMAGE=IMAGES[args.engine])
    # Acquisition policy stays never. CI must preload the two explicit image
    # digests; a missing fixture is a failure rather than an implicit pull.
    for image in {IMAGES[args.engine], IMAGES["pg"]}:
        subprocess.run(["docker", "--host", f"unix://{socket}", "image", "inspect", image],
                       check=True, stdout=subprocess.DEVNULL)
    subprocess.run([sys.executable, "scripts/resolver-daemon-fixture.py"], check=True,
                   env=dict(env, DOCKER_HOST=f"unix://{socket}", DOCKER_CONTEXT=""))

    def run(*args, **kwargs):
        if args[0] == "docker":
            args = ("docker", "--host", f"unix://{socket}", *args[1:])
        kwargs.setdefault("text", True)
        return subprocess.run(args, **kwargs)

    # Before the tests, not after a failure: a reporter that printed nothing
    # would be discovered by the failure it exists to explain (#724).
    self_check(run, IMAGES[args.engine])
    for test in TESTS:
        command = ["cargo", "test", "-p", "pbps-cli", "--lib", "--", "--ignored",
                   "--exact", "resolver::" + test, "--nocapture"]
        completed = subprocess.run(command, env=env, text=True,
                                   stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        print(completed.stdout, end="", flush=True)
        if completed.returncode or "test result: ok. 1 passed" not in completed.stdout:
            # Each test owns and removes its containers, so a start that
            # failed has already taken its engine's log with it unless it is
            # read here. `recovery_names` is what the failure carries for
            # exactly this: the names it could not confirm were removed.
            for container in _recovery_names(completed.stdout):
                report(run, container)
                run("docker", "rm", "-f", container, check=False,
                    stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            raise SystemExit("resolver fixture must run exactly one passing test: " + test)


def _recovery_names(output):
    """The container names a failed start could not confirm removed.

    Read out of the test's own `StartFailure { .. recovery_names: [..] }`,
    because the fixture does not otherwise know what the test created. Only
    generated fixture names are ever acted on — the resolver's own prefixes —
    so a malformed or unexpected line removes nothing rather than something
    else's container.
    """
    names = []
    for quoted in re.findall(r'recovery_names: \[([^\]]*)\]', output):
        for name in re.findall(r'"([^"]+)"', quoted):
            if name.startswith("pbps-resolver-") and name not in names:
                names.append(name)
    return names


if __name__ == "__main__":
    main()
