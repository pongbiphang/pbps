#!/usr/bin/env bash
# Runs the live SQL Server tests (SPEC §11.5) against a throwaway container.
#
# The container is left running afterwards so re-runs are instant; remove it
# with: docker rm -f pbps-test-mssql
set -euo pipefail

NAME=pbps-test-mssql
# Overridable: a developer machine often already has a SQL Server container
# holding the default port, and "the tests will not start" is a bad way to find
# that out.
PORT=${PBPS_TEST_PORT:-14330}
PASSWORD='Pbps!Test12345'
# Pinned by digest, and the same one CI uses: SQL Server 2025 RTM, 17.0.4075.5,
# image built 2026-07-23. A floating tag would let the engine under test change
# between two runs of the same commit, which is the one thing a suite that
# exists to answer "what does the engine actually do" cannot afford.
#
# An existing `pbps-test-mssql` is reused as-is, so a container started from an
# earlier image keeps running; `docker rm -f pbps-test-mssql` to move it onto
# this digest.
IMAGE=mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker run -d --name "$NAME" -e ACCEPT_EULA=Y \
        -e "MSSQL_SA_PASSWORD=$PASSWORD" -p "$PORT:1433" "$IMAGE" >/dev/null
fi

echo "waiting for SQL Server..."
for _ in $(seq 1 60); do
    if docker exec "$NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
        -U sa -P "$PASSWORD" -Q "SELECT 1" >/dev/null 2>&1; then
        break
    fi
    sleep 2
done

export PBPS_TEST_DB="Server=localhost,$PORT;User Id=sa;Password=$PASSWORD;TrustServerCertificate=true"

# The `--dev docker://` path starts a *second*, throwaway server of its own, so
# it stays opt-in: naming an image here is what enables it. CI covers it in a
# job of its own (`dev-rehearsal`) rather than alongside the service container,
# for the same reason it is opt-in here — two engines on one machine is a cost
# worth paying deliberately.
#
# It is worth covering at all because that path had no automated coverage
# whatsoever, which is how `Container::start` shipped removing the container it
# had just returned: `plan --dev docker://...` failed with "connection refused"
# for every user, and no test ran it.
export PBPS_TEST_DEV_IMAGE="${PBPS_TEST_DEV_IMAGE:-$IMAGE}"

# The dialect's live tests, then the CLI's dev-database rehearsal (SPEC §9.3),
# which needs the same server and is `#[ignore]`d for the same reason.
cargo test -p pbps-mssql --test live -- --ignored "$@"
# `--test-threads=1`: these share one SQL Server, and the deployment lock is a
# single row in it. Two tests taking it concurrently make each other fail, and
# the failure reads as a bug in the lock rather than in the test schedule.
#
# On macOS two tests are `ignore`d for a different reason — APFS cannot hold a
# filename that is not valid UTF-8 — and `--ignored` would force them anyway.
# They are skipped by name here only where they cannot run; on Linux they are
# not ignored, run in the ordinary suite, and this filter never sees them. If
# a rename stops the filter matching, the tests run and fail loudly on macOS,
# which is the safe direction for a name filter to be wrong in.
SKIP=()
if [ "$(uname -s)" = Darwin ]; then
    SKIP=(--skip not_utf8 --skip non_utf8)
fi
exec cargo test -p pbps-cli --test flow -- --ignored --test-threads=1 "${SKIP[@]}" "$@"
