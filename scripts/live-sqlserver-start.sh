# Sourced by scripts/live-tests.sh: how it starts the SQL Server fixtures it owns.
#
# SQL Server 2025 sometimes dumps core within seconds of starting when the host
# is busy, as often after it answered a login as before (#2875, DEC-2875.1,
# which extends DEC-958.1). A fixture this run started is held for a short
# window after it is ready; one that exited with a `core.sqlservr` dump in its
# log is reported, removed and started again, at most twice. Any other exit, a
# third crash and a fixture that was already there fail as before. Nothing is
# retried once the tests run: this is only the start-up.
#
# The variables below belong to the caller and are read when a function runs:
# NAME, PORT, PASSWORD, IMAGE, EXPRESS_NAME and EXPRESS_PORT.
SURVIVAL_SECONDS=${PBPS_TEST_MSSQL_SURVIVAL_SECONDS:-15}

# True only when the daemon says the container is not running: a failed
# inspect is not an answer, and the poll goes on.
sqlserver_stopped() {
    [[ $(docker inspect --format '{{.State.Running}}' "$1" 2>/dev/null) == false ]]
}

# sqlserver_bring_up <name> <start-function> <ready-function>
sqlserver_bring_up() {
    local name=$1 start=$2 ready=$3 attempt=1 status
    while :; do
        "$start"
        "$ready"
        status=0
        python3 scripts/check-live-fixture.py --survive "$SURVIVAL_SECONDS" --container "$name" || status=$?
        case $status in
            0) return 0 ;;
            75) ;;
            *) return "$status" ;;
        esac
        if (( attempt >= 3 )); then
            # The admission that follows refuses the stopped fixture as it
            # always has.
            echo "$name crashed with a core dump during start-up $attempt times; not starting it again" >&2
            return 0
        fi
        echo "$name crashed with a core dump during start-up (attempt $attempt of 3, log above); removing it and starting a fresh one" >&2
        docker rm -f -v "$name" >/dev/null
        attempt=$((attempt + 1))
    done
}

start_primary() {
    docker run -d --name "$NAME" -e ACCEPT_EULA=Y \
        -e "MSSQL_SA_PASSWORD=$PASSWORD" -p "$PORT:1433" "$IMAGE" >/dev/null
}

primary_ready() {
    echo "waiting for SQL Server..."
    for _ in $(seq 1 60); do
        if docker exec "$NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
            -U sa -P "$PASSWORD" -Q "SELECT 1" >/dev/null 2>&1; then
            break
        fi
        # An engine that already exited will not answer in two minutes either.
        ! sqlserver_stopped "$NAME" || break
        sleep 2
    done
}

start_express() {
    docker run -d --name "$EXPRESS_NAME" -e ACCEPT_EULA=Y -e MSSQL_PID=Express \
        -e MSSQL_COLLATION=Latin1_General_100_CS_AS \
        -e "MSSQL_SA_PASSWORD=$PASSWORD" -p "$EXPRESS_PORT:1433" "$IMAGE" >/dev/null
}

express_ready_probe() {
    echo "waiting for the Express SQL Server..."
    # A successful login precedes the asynchronous custom collation change.
    express_ready=false
    for _ in $(seq 1 60); do
        if docker exec "$EXPRESS_NAME" /opt/mssql-tools18/bin/sqlcmd -C -S localhost \
            -U sa -P "$PASSWORD" -b -Q "IF COALESCE(CONVERT(varchar(128), SERVERPROPERTY('Collation')), '') <> 'Latin1_General_100_CS_AS' THROW 50000, 'fixture collation is not ready', 1;" >/dev/null 2>&1; then
            express_ready=true
            break
        fi
        ! sqlserver_stopped "$EXPRESS_NAME" || break
        sleep 2
    done
}
