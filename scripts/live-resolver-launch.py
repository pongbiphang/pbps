#!/usr/bin/env python3
"""Verify launch privilege refusal before initializing an owned scratch engine.

Uses the explicit prebuilt CLI test binary on a disposable native Linux host.
Each test creates and confirms removal of its own containers. Reading the
actual root guard and dropped waiter requires root, like the factory fixture.
"""

import argparse
import os
from pathlib import Path
import subprocess
import sys

IMAGES = {
    "pg": "postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280",
    "mssql": "mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1",
}
TESTS = [
    "a_wrong_workload_identity_is_refused_before_initialization",
    "a_guard_without_termination_authority_is_refused_before_initialization",
    "the_launch_cannot_omit_inherited_no_new_privileges",
    "effective_descriptor_limits_are_required_before_initialization",
    "unreadable_effective_limits_are_not_a_bounded_answer",
    "masks::every_existing_proc_interface_requires_its_effective_mask",
]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("engine", choices=IMAGES)
    parser.add_argument("--socket", default="/var/run/docker.sock")
    parser.add_argument("--test-binary", required=True)
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() != 0:
        parser.error("requires root on the same native Linux host as the fixture daemon")
    binary = Path(args.test_binary)
    if not binary.is_absolute() or not binary.is_file() or not Path(args.socket).is_absolute():
        parser.error("requires an absolute existing test binary and absolute Docker socket")
    env = dict(os.environ, PBPS_RESOLVER_TEST_SOCKET=args.socket,
               PBPS_RESOLVER_TEST_DRIVER=args.engine,
               PBPS_RESOLVER_TEST_IMAGE=IMAGES[args.engine])
    for name in TESTS:
        command = [str(binary), "--ignored", "--exact",
                   "resolver::docker::profile::launch_tests::" + name, "--nocapture"]
        selected = env
        if name == "unreadable_effective_limits_are_not_a_bounded_answer":
            command = ["unshare", "--mount", "--propagation", "private", "--", *command]
            selected = dict(env, PBPS_LIMITS_PRIVATE_PROC_FIXTURE="1")
        if name.startswith("masks::"):
            # Root can still observe the guard with SYS_PTRACE, but must not
            # bypass a mask directory's absent read/search permissions.
            command = ["setpriv", "--bounding-set=-dac_override,-dac_read_search", "--", *command]
        result = subprocess.run(
            command,
            env=selected, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        print(result.stdout, end="", flush=True)
        if result.returncode or "test result: ok. 1 passed" not in result.stdout:
            raise SystemExit("launch fixture must run exactly one passing test: " + name)


if __name__ == "__main__":
    main()
