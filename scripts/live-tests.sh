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
IMAGE=mcr.microsoft.com/mssql/server:2025-latest

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
# it is opt-in rather than always-on: naming an image here is what enables it.
# Set locally and deliberately left out of CI's live job, which already runs one
# SQL Server as a service container — a second on the same runner is a memory
# and flakiness cost that belongs in a CI decision of its own, not at the tail of
# a feature branch.
#
# It is worth having at all because that path had no automated coverage
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
exec cargo test -p pbps-cli --test flow -- --ignored --test-threads=1 "$@"
