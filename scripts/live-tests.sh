#!/usr/bin/env bash
# Runs the live SQL Server tests (SPEC §11.5) against a throwaway container.
#
# The containers are left running afterwards so re-runs are instant; remove
# them with: docker rm -f "${PBPS_TEST_MSSQL_NAME:-pbps-test-mssql}" \
#   "${PBPS_TEST_MSSQL_EXPRESS_NAME:-pbps-test-mssql-express}" \
#   "${PBPS_TEST_MSSQL_SCRATCH_NAME:-${PBPS_TEST_MSSQL_NAME:-pbps-test-mssql}-scratch}"
set -euo pipefail

if [[ -n ${PBPS_LIVE_SHARD:-} && ${PBPS_LIVE_ACTIVE:-} != 1 ]]; then
    exec python3 scripts/live-shards.py shard mssql "$@"
fi

NAME=${PBPS_TEST_MSSQL_NAME:-pbps-test-mssql}
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
# Existing fixtures must match the pin and endpoint before client tests run.
# A stale fixture is refused; its owner decides when to replace it.
IMAGE=mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1

containers=$(docker ps -a --format '{{.Names}}')
if ! grep -Fxq -- "$NAME" <<< "$containers"; then
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

# A second instance of the same build, as another edition and under another
# server collation: what the resolver's analysis-scope rule is measured
# against for the two facts no database on the first server can vary (#611).
# Express, so it stays small beside the first one.
EXPRESS_NAME=${PBPS_TEST_MSSQL_EXPRESS_NAME:-pbps-test-mssql-express}
EXPRESS_PORT=${PBPS_TEST_EXPRESS_PORT:-14331}
if [[ ${PBPS_LIVE_SHARD:-0/1} == 0/* ]]; then
    containers=$(docker ps -a --format '{{.Names}}')
    if ! grep -Fxq -- "$EXPRESS_NAME" <<< "$containers"; then
        docker run -d --name "$EXPRESS_NAME" -e ACCEPT_EULA=Y -e MSSQL_PID=Express \
            -e MSSQL_COLLATION=Latin1_General_100_CS_AS \
            -e "MSSQL_SA_PASSWORD=$PASSWORD" -p "$EXPRESS_PORT:1433" "$IMAGE" >/dev/null
    fi
    echo "waiting for the Express SQL Server..."
    # A successful login precedes the asynchronous custom collation change.
    express_ready=false
    for _ in $(seq 1 60); do
        if docker exec "$EXPRESS_NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
            -U sa -P "$PASSWORD" -b -Q "IF COALESCE(CONVERT(varchar(128), SERVERPROPERTY('Collation')), '') <> 'Latin1_General_100_CS_AS' THROW 50000, 'fixture collation is not ready', 1;" >/dev/null 2>&1; then
            express_ready=true
            break
        fi
        sleep 2
    done
    if [[ "$express_ready" != true ]]; then
        echo "$EXPRESS_NAME did not reach its configured collation" >&2
        docker logs "$EXPRESS_NAME" >&2 || true
        exit 1
    fi
    export PBPS_TEST_EXPRESS_DB="Server=localhost,$EXPRESS_PORT;User Id=sa;Password=$PASSWORD;TrustServerCertificate=true"

else
    unset PBPS_TEST_EXPRESS_DB
fi

python3 scripts/check-live-fixture.py --cell mssql2025 --container "$NAME" --port "$PORT"
if [[ ${PBPS_LIVE_SHARD:-0/1} == 0/* ]]; then
    python3 scripts/check-live-fixture.py --cell mssql2025-express --container "$EXPRESS_NAME" --port "$EXPRESS_PORT"
fi

source scripts/resolver-mssql-fixtures.sh

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
cargo test -p pbps-mssql --test live -- --ignored --skip scratch2236 --skip deployer2267 --skip staged2268 --skip externals2269 "$@"
cargo test --profile live-test -p pbps-mssql --test live -- --ignored --test-threads=1 scratch2236 "$@"
cargo test --profile live-test -p pbps-mssql --test live -- --ignored --test-threads=1 deployer2267 "$@"
cargo test --profile live-test -p pbps-mssql --test live -- --ignored --test-threads=1 staged2268 "$@"
cargo test --profile live-test -p pbps-mssql --test live -- --ignored --test-threads=1 externals2269 "$@"
cargo test --profile live-test -p pbps-cli --lib -- --ignored --test-threads=1 mssql_live_tests "$@"
cargo test --profile live-test -p pbps-cli --bin pbps -- --ignored --test-threads=1 engine::mssql_live_tests "$@"
# The seam's own error conversion on this driver (issue #167): measured
# against a real server, `tiberius`'s `Display` already carries the server's
# sentence, so this is a regression guard rather than a fix — see
# `crates/pbps-db/src/mssql.rs` for the comparison against the PostgreSQL side.
cargo test -p pbps-db --test live_mssql -- --ignored "$@"
if [[ -z ${PBPS_LIVE_SHARD:-} ]]; then
    python3 scripts/live-transport.py mssql
elif [[ ${PBPS_LIVE_SHARD%%/*} == 0 ]]; then
    PBPS_LIVE_PINNED=1 python3 scripts/live-transport.py mssql
fi
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
# `${SKIP[@]+"${SKIP[@]}"}`, not `"${SKIP[@]}"`: under `set -u` an empty array
# is an unbound variable in bash before 4.4, and macOS ships 3.2.
exec cargo test -p pbps-cli --test flow -- --ignored --test-threads=1 ${SKIP[@]+"${SKIP[@]}"} "$@"
