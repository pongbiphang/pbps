#!/bin/sh
# install-pg-partman.sh <package>: run INSIDE a pinned PostgreSQL fixture
# container (`docker exec -i <name> sh -s -- postgresql-<major>-partman < this-file`) to add
# pg_partman for `partitions: external` (#1533) without touching the running
# server.
#
# Why not `apt-get install`: the package depends on the repository's newest
# server release (postgresql-<major> > the pinned one). apt satisfies that by
# upgrading the server packages underneath the process that is already
# running, and every later DO block, extension or subscription then fails with
# "could not load library plpgsql.so: undefined symbol" (#2835). The pin
# check (check-live-fixture.py) cannot see it: the old process still reports
# the pinned version. So the .deb is downloaded and unpacked past its
# dependency check, and the server package version is compared afterwards so
# the fault cannot come back silently.
set -eu
pkg=$1
# The server package the extension package is built for.
server=${pkg%-partman}

# Idempotent: a re-run against a container that already has it does nothing.
dpkg -s "$pkg" >/dev/null 2>&1 && exit 0

# apt-get download writes into the working directory, which must be writable
# (the container's own is not always).
cd "${TMPDIR:-/tmp}"
before=$(dpkg-query -W "$server")
apt-get update -qq
apt-get download -qq "$pkg"
dpkg --force-depends -i "${pkg}_"*.deb
after=$(dpkg-query -W "$server")
if [ "$before" != "$after" ]; then
    echo "installing $pkg changed the server package: '$before' -> '$after'" >&2
    exit 1
fi
