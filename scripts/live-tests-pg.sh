#!/usr/bin/env bash
# Runs the live PostgreSQL tests (SPEC §11.5) against a throwaway container.
#
# The counterpart of `live-tests.sh`, and separate from it on purpose: the two
# suites answer questions about two engines, and a developer changing one
# dialect should not have to start the other engine to run its tests.
#
# The container is left running afterwards so re-runs are instant; remove it
# with: docker rm -f pbps-test-pg
set -euo pipefail

NAME=pbps-test-pg
# Overridable for the same reason as the SQL Server one: a developer machine
# often already has PostgreSQL on 5432, and "the tests will not start" is a bad
# way to find that out.
PORT=${PBPS_TEST_PG_PORT:-54320}
PASSWORD='Pbps!Test12345'
DB=pbps_test
# Pinned by digest, and the same one CI uses: PostgreSQL 18.6, Debian
# 18.6-1.pgdg13+2. A floating tag would let the engine under test change
# between two runs of the same commit, which is the one thing a suite that
# exists to answer "what does the engine actually do" cannot afford.
#
# An existing `pbps-test-pg` is reused as-is, so a container started from an
# earlier image keeps running; `docker rm -f pbps-test-pg` to move it onto
# this digest.
IMAGE=postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280

if ! docker ps --format '{{.Names}}' | grep -qx "$NAME"; then
    docker rm -f "$NAME" >/dev/null 2>&1 || true
    docker run -d --name "$NAME" \
        -e "POSTGRES_PASSWORD=$PASSWORD" -e "POSTGRES_DB=$DB" \
        -p "$PORT:5432" "$IMAGE" >/dev/null
fi

echo "waiting for PostgreSQL..."
for _ in $(seq 1 60); do
    if docker exec "$NAME" pg_isready -U postgres -d "$DB" >/dev/null 2>&1; then
        break
    fi
    sleep 2
done

# A libpq keyword string rather than a URL: both are accepted, and this one
# does not need the password percent-encoded.
export PBPS_TEST_PG_DB="host=localhost port=$PORT user=postgres password=$PASSWORD dbname=$DB"

# One of these tests waits out `pbps_db::CONNECT_TIMEOUT` on purpose — a
# firewall that drops rather than refuses is a category of its own, and thirty
# seconds is what proves the wait is bounded. It is why this suite takes half a
# minute for six tests.
cargo test -p pbps-pg --test live -- --ignored "$@"
