#!/usr/bin/env bash
# Run one finite compatibility cell. Reused fixtures must pass admission.
set -euo pipefail
cd "$(dirname "$0")/.."
export PBPS_COMPAT_CELL=${1:?usage: compatibility-tests.sh CELL}
# Read only known fields; unknown cells fail before any Docker operation.
image=$(python3 -c 'import json,sys; print(json.load(open("scripts/compatibility-matrix.json"))[sys.argv[1]]["image"])' "$PBPS_COMPAT_CELL")
name=${PBPS_COMPAT_CONTAINER:-pbps-compat-$PBPS_COMPAT_CELL}
port=${PBPS_COMPAT_PORT:-15432}
password='Pbps!Test12345'
containers=$(docker ps -a --format '{{.Names}}')
case "$PBPS_COMPAT_CELL" in
    pg16|pg18)
        if ! grep -Fxq "$name" <<< "$containers"; then
            docker run -d --name "$name" -e "POSTGRES_PASSWORD=$password" \
                -p "127.0.0.1:$port:5432" "$image" >/dev/null
        fi
        ready=false
        for _ in $(seq 1 90); do
            if docker exec -e "PGPASSWORD=$password" "$name" psql -h 127.0.0.1 -U postgres -d postgres -Atc 'SELECT 1' >/dev/null 2>&1; then
                ready=true; break
            fi
            sleep 2
        done
        export PBPS_TEST_PG_DB="host=127.0.0.1 port=$port user=postgres password=$password dbname=postgres sslmode=disable"
        target=flow_pg
        ;;
    mssql2022|mssql2025|mssql2025-express)
        edition=EnterpriseDeveloper
        collation=SQL_Latin1_General_CP1_CI_AS
        if [[ "$PBPS_COMPAT_CELL" == mssql2022 ]]; then edition=Developer; fi
        if [[ "$PBPS_COMPAT_CELL" == mssql2025-express ]]; then
            edition=Express; collation=Latin1_General_100_CS_AS
        fi
        if ! grep -Fxq "$name" <<< "$containers"; then
            docker run -d --name "$name" -e ACCEPT_EULA=Y -e "MSSQL_PID=$edition" \
                -e "MSSQL_COLLATION=$collation" -e "MSSQL_SA_PASSWORD=$password" \
                -p "127.0.0.1:$port:1433" "$image" >/dev/null
        fi
        ready=false
        for _ in $(seq 1 90); do
            if docker exec "$name" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
                -U sa -P "$password" -b -Q 'SELECT 1' >/dev/null 2>&1; then
                ready=true; break
            fi
            sleep 2
        done
        export PBPS_TEST_DB="Server=127.0.0.1,$port;User Id=sa;Password=$password;TrustServerCertificate=true"
        target=flow
        ;;
    *) echo 'unknown compatibility cell' >&2; exit 1 ;;
esac
if [[ "$ready" != true ]]; then
    echo 'compatibility fixture did not become ready' >&2
    docker logs "$name" >&2 || true
    exit 1
fi
python3 scripts/check-live-fixture.py --cell "$PBPS_COMPAT_CELL" --container "$name" --port "$port"
cargo test -p pbps-cli --test "$target" compatibility_core_contract -- --ignored --exact --nocapture
