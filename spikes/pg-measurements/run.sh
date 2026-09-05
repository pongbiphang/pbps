#!/usr/bin/env bash
# Re-runs the measurements behind ADR-0009 through ADR-0014 and diffs them
# against what was observed when those documents were written.
#
#   ./run.sh              both engines, containers started and removed here
#   ./run.sh postgres     just PostgreSQL
#   ./run.sh sqlserver    just SQL Server
#
# Engines are pinned by digest, as scripts/live-tests.sh pins its own: a suite
# whose job is to answer "what does the engine actually do" cannot have the
# engine change under it between two runs. Bump deliberately, and expect the
# diff to be the point.
set -euo pipefail
cd "$(dirname "$0")"

PG_IMAGE=${PG_IMAGE:-docker.io/library/postgres@sha256:4ef4dbc939d61acea57712655ddb4b4ab27419c913f94cca0cd57cb3ea3c2280}
MSSQL_IMAGE=${MSSQL_IMAGE:-mcr.microsoft.com/mssql/server@sha256:4bab24f36c1ecd48e85f7d37df26e6bf301641d84c3fe652f9a0dcc947d512e1}
PG_PORT=${PG_PORT:-55432}
MSSQL_PORT=${MSSQL_PORT:-14331}
RUNTIME=${RUNTIME:-podman}
SA_PASSWORD=${SA_PASSWORD:-'Probe_Passw0rd!'}

run_postgres() {
  local name=pbps-measure-pg
  $RUNTIME rm -f $name >/dev/null 2>&1 || true
  $RUNTIME run -d --name $name -e POSTGRES_PASSWORD=probe -p "$PG_PORT:5432" "$PG_IMAGE" >/dev/null
  trap "$RUNTIME rm -f $name >/dev/null 2>&1 || true" RETURN
  for _ in $(seq 1 60); do
    $RUNTIME exec $name pg_isready -U postgres >/dev/null 2>&1 && break
    sleep 1
  done
  $RUNTIME exec $name psql -U postgres -tAc 'select version()' | sed 's/^/-- /'
  $RUNTIME exec -i $name psql -U postgres -X -q -f - < postgres.sql 2>&1 \
    | grep -E '^[ABTR][0-9-]' || { echo "no measurements produced" >&2; return 1; }
}

run_sqlserver() {
  local name=pbps-measure-mssql
  $RUNTIME rm -f $name >/dev/null 2>&1 || true
  $RUNTIME run -d --name $name -e ACCEPT_EULA=Y -e MSSQL_SA_PASSWORD="$SA_PASSWORD" \
    -p "$MSSQL_PORT:1433" "$MSSQL_IMAGE" >/dev/null
  trap "$RUNTIME rm -f $name >/dev/null 2>&1 || true" RETURN
  local sqlcmd=/opt/mssql-tools18/bin/sqlcmd
  for _ in $(seq 1 90); do
    $RUNTIME exec $name $sqlcmd -S localhost -U sa -P "$SA_PASSWORD" -C -Q 'SELECT 1' >/dev/null 2>&1 && break
    sleep 2
  done
  $RUNTIME exec $name $sqlcmd -S localhost -U sa -P "$SA_PASSWORD" -C -h -1 -W \
    -Q "SELECT LEFT(@@VERSION, CHARINDEX(CHAR(10), @@VERSION) - 1)" | sed 's/^/-- /' | head -1
  $RUNTIME exec -i $name $sqlcmd -S localhost -U sa -P "$SA_PASSWORD" -C -h -1 -W -i /dev/stdin \
    < sqlserver.sql 2>&1 | grep -E '^M[0-9]' || { echo "no measurements produced" >&2; return 1; }
}

case "${1:-both}" in
  postgres)  run_postgres  | tee observed-postgres.txt ;;
  sqlserver) run_sqlserver | tee observed-sqlserver.txt ;;
  both)      run_postgres  | tee observed-postgres.txt
             run_sqlserver | tee observed-sqlserver.txt ;;
  *) echo "usage: $0 [postgres|sqlserver|both]" >&2; exit 2 ;;
esac

git diff --stat -- observed-postgres.txt observed-sqlserver.txt 2>/dev/null | grep . \
  && echo "^^ an engine answered differently than when the ADRs were written. That is the signal." \
  || echo "-- every answer matches what the ADRs record."
