#!/bin/sh
# live-pg-nondurable.sh: run INSIDE a PostgreSQL live-test fixture container
# (`docker exec -i <name> sh -s < scripts/live-pg-nondurable.sh`) to turn off
# the three durability settings of a server whose data is disposable (#2864):
# `fsync`, `synchronous_commit` and `full_page_writes`. Measured on 70
# `flow_pg` cases, the suite waits mostly on commits and spent 16% less wall
# time with them off (DEC-2864.1).
#
# Why a script run after start, not `-c fsync=off` on the server command line:
# a GitHub Actions `services:` container accepts no server arguments, and the
# local runner and CI should set the fixtures up the same way. All three are
# reloadable (`fsync` and `full_page_writes` on SIGHUP, `synchronous_commit` per
# session), so `ALTER SYSTEM` plus a reload needs no restart. It persists in
# the data directory, so a clean restart keeps it.
#
# It must fail loudly: the values are read back, and an unreadable result is a
# failure, not "probably off". A fixture left durable would only be slower, but
# one the suite believes is non-durable and is not would make a later
# measurement of the gain read as noise.
#
# Never point it at a server that holds data worth keeping: with `fsync` off a
# crash can corrupt the cluster.
set -eu

# Reload is asynchronous: the postmaster applies SIGHUP a moment after
# pg_reload_conf() returns, so a new session may still see the old value.
tries=${PBPS_FIXTURE_SETTLE_TRIES:-30}
settings="fsync synchronous_commit full_page_writes"

sql() {
    # The local socket as the superuser: no password, and `postgres` exists on
    # every image, unlike the database the image was asked to create.
    psql -X -q -At -U postgres -d postgres -v ON_ERROR_STOP=1 -c "$1"
}

# `ALTER SYSTEM` cannot run in a transaction block, so one statement per call.
for setting in $settings; do
    sql "ALTER SYSTEM SET $setting = off" >/dev/null
done
sql "SELECT pg_reload_conf()" >/dev/null

attempt=0
while :; do
    attempt=$((attempt + 1))
    wrong=
    for setting in $settings; do
        # A failed or empty read is "unreadable", never "off".
        value=$(sql "SELECT setting FROM pg_settings WHERE name = '$setting'" 2>/dev/null) || value=
        if [ "$value" != off ]; then
            wrong="$wrong $setting=${value:-unreadable}"
        fi
    done
    [ -z "$wrong" ] && exit 0
    if [ "$attempt" -ge "$tries" ]; then
        echo "live-pg-nondurable: settings did not take effect:$wrong" >&2
        exit 1
    fi
    sleep 1
done
