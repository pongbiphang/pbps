#!/usr/bin/env bash
# Source this file: both connection variables belong to the calling runner.
# These fixtures share one immutable image and the primary's server collation.
# Remove them with: docker rm -f "$PBPS_RM_NAME" "$PBPS_RM_SCRATCH_NAME"
set -euo pipefail

PBPS_RM_NAME=${NAME:-${PBPS_TEST_MSSQL_NAME:-pbps-test-mssql}}
PBPS_RM_SCRATCH_NAME=${PBPS_TEST_MSSQL_SCRATCH_NAME:-$PBPS_RM_NAME-scratch}
PBPS_RM_IMAGE=mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1
PBPS_RM_PASSWORD='Pbps!Test12345'
PBPS_RM_PORT=${PBPS_TEST_PORT:-14330}
PBPS_RM_SCRATCH_PORT=${PBPS_TEST_MSSQL_SCRATCH_PORT:-14332}

pbps_rm_wait() {
    local pbps_rm_name=$1
    for _ in $(seq 1 90); do
        if docker exec "$pbps_rm_name" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
            -U sa -P "$PBPS_RM_PASSWORD" -b -Q 'SELECT 1 AS ok' >/dev/null 2>&1; then
            return 0
        fi
        sleep 2
    done
    docker logs "$pbps_rm_name" >&2 || true
    return 1
}

PBPS_RM_CONTAINERS=$(docker ps -a --format '{{.Names}}')
if ! grep -Fxq -- "$PBPS_RM_NAME" <<< "$PBPS_RM_CONTAINERS"; then
    docker run -d --name "$PBPS_RM_NAME" -e ACCEPT_EULA=Y \
        -e "MSSQL_SA_PASSWORD=$PBPS_RM_PASSWORD" \
        -p "127.0.0.1:$PBPS_RM_PORT:1433" "$PBPS_RM_IMAGE" >/dev/null
fi
pbps_rm_wait "$PBPS_RM_NAME"
PBPS_RM_COLLATION=$(docker exec "$PBPS_RM_NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
    -U sa -P "$PBPS_RM_PASSWORD" -b -h -1 -W \
    -Q "SET NOCOUNT ON; SELECT CONVERT(varchar(128),SERVERPROPERTY('Collation'));")
PBPS_RM_COLLATION=${PBPS_RM_COLLATION//$'\r'/}
if [[ ! "$PBPS_RM_COLLATION" =~ ^[A-Za-z0-9_]+$ ]]; then
    echo 'The primary SQL Server did not report one server collation' >&2
    return 1
fi
if ! grep -Fxq -- "$PBPS_RM_SCRATCH_NAME" <<< "$PBPS_RM_CONTAINERS"; then
    docker run -d --name "$PBPS_RM_SCRATCH_NAME" -e ACCEPT_EULA=Y \
        -e "MSSQL_COLLATION=$PBPS_RM_COLLATION" -e "MSSQL_SA_PASSWORD=$PBPS_RM_PASSWORD" \
        -p "127.0.0.1:$PBPS_RM_SCRATCH_PORT:1433" "$PBPS_RM_IMAGE" >/dev/null
fi
pbps_rm_wait "$PBPS_RM_SCRATCH_NAME"
# A login can succeed before an asynchronous collation change finishes.
PBPS_RM_READY=false
for _ in $(seq 1 90); do
    if docker exec "$PBPS_RM_SCRATCH_NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
        -U sa -P "$PBPS_RM_PASSWORD" -b \
        -Q "IF CONVERT(varchar(128),SERVERPROPERTY('Collation')) <> '$PBPS_RM_COLLATION' THROW 50000, 'scratch collation is not ready', 1;" >/dev/null 2>&1; then
        PBPS_RM_READY=true
        break
    fi
    sleep 2
done
if [[ "$PBPS_RM_READY" != true ]]; then
    docker logs "$PBPS_RM_SCRATCH_NAME" >&2 || true
    return 1
fi
python3 scripts/check-live-fixture.py --cell mssql2025 --container "$PBPS_RM_NAME" --port "$PBPS_RM_PORT"
python3 scripts/check-live-fixture.py --cell mssql2025 \
    --container "$PBPS_RM_SCRATCH_NAME" --port "$PBPS_RM_SCRATCH_PORT"
export PBPS_TEST_DB="Server=localhost,$PBPS_RM_PORT;User Id=sa;Password=$PBPS_RM_PASSWORD;TrustServerCertificate=true"
export PBPS_TEST_MSSQL_SCRATCH_DB="Server=localhost,$PBPS_RM_SCRATCH_PORT;User Id=sa;Password=$PBPS_RM_PASSWORD;TrustServerCertificate=true"
export PBPS_TEST_RESOLVER_MSSQL_IMAGE="$PBPS_RM_IMAGE"
