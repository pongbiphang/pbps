#!/usr/bin/env bash
# Runs the live PostgreSQL tests (SPEC §11.5) against a throwaway container.
#
# The counterpart of `live-tests.sh`, and separate from it on purpose: the two
# suites answer questions about two engines, and a developer changing one
# dialect should not have to start the other engine to run its tests.
#
# The containers are left running afterwards so re-runs are instant; remove them
# with: docker rm -f pbps-test-pg pbps-test-pg16
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

# A second server, older than PostgreSQL 17, and it earns its cost: the
# `maintain` permission arrived in 17, so "this engine refuses the word" and
# "this engine takes it" are two different servers and no single one can show
# both (ADR-0010 §6, amendment). Measured on this digest: PostgreSQL 16.15
# (Debian 16.15-1.pgdg13+2), `server_version_num` 160015, where
# `GRANT MAINTAIN ON t TO r` is `unrecognized privilege type "maintain"` and
# the owner's default relation ACL is `arwdDxt` — no `m`.
#
# Bump it only to another release **below 17**; a bump past that would make the
# test that asserts the refusal pass for no reason.
OLD_NAME=pbps-test-pg16
OLD_PORT=${PBPS_TEST_PG_OLD_PORT:-54321}
OLD_IMAGE=postgres@sha256:485935f94cc7165afa896978809c37b592dc07f0a37d2c8f645f12412d0212c8

start() {
    local name=$1 port=$2 image=$3
    if ! docker ps --format '{{.Names}}' | grep -qx "$name"; then
        docker rm -f "$name" >/dev/null 2>&1 || true
        docker run -d --name "$name" \
            -e "POSTGRES_PASSWORD=$PASSWORD" -e "POSTGRES_DB=$DB" \
            -p "$port:5432" "$image" >/dev/null
    fi
}

start "$NAME" "$PORT" "$IMAGE"
start "$OLD_NAME" "$OLD_PORT" "$OLD_IMAGE"

echo "waiting for PostgreSQL..."
for name in "$NAME" "$OLD_NAME"; do
    for _ in $(seq 1 60); do
        if docker exec "$name" pg_isready -U postgres -d "$DB" >/dev/null 2>&1; then
            break
        fi
        sleep 2
    done
done

# A libpq keyword string rather than a URL: both are accepted, and this one
# does not need the password percent-encoded.
export PBPS_TEST_PG_DB="host=localhost port=$PORT user=postgres password=$PASSWORD dbname=$DB"
export PBPS_TEST_PG_OLD_DB="host=localhost port=$OLD_PORT user=postgres password=$PASSWORD dbname=$DB"

# One of these tests waits out `pbps_db::CONNECT_TIMEOUT` on purpose — a
# firewall that drops rather than refuses is a category of its own, and thirty
# seconds is what proves the wait is bounded. It is why this suite takes half a
# minute for six tests.
cargo test -p pbps-pg --test live -- --ignored "$@"
# And the CLI's own PostgreSQL-dependent test, which lives in the bin target
# rather than in `tests/`: `deploy::preflight` and `deploy::run_probes` are
# private, and the thing under test is that the second pins the session before
# it asks anything (DECISIONS 415). Run here rather than left to
# `cargo test --workspace`, which does not pass `--ignored`.
cargo test -p pbps-cli --bin pbps -- --ignored "$@"
# The CLI end to end on this engine, serially: each test creates and drops a
# database of its own, and two of those racing is a race the engine can lose.
cargo test -p pbps-cli --test flow_pg -- --ignored --test-threads=1 "$@"
