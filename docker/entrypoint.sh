#!/bin/sh
# Run subsyncer as PUID:PGID (default 1000:1000) so written subtitles keep
# the ownership your media library expects. If the container is already
# started as a non-root user (docker --user), run as that user directly.
set -e

if [ "$(id -u)" = "0" ]; then
    PUID="${PUID:-1000}"
    PGID="${PGID:-1000}"
    if [ "$(id -g subsyncer)" != "$PGID" ]; then
        groupmod -o -g "$PGID" subsyncer
    fi
    if [ "$(id -u subsyncer)" != "$PUID" ]; then
        usermod -o -u "$PUID" subsyncer
    fi
    data="${SUBSYNCER_DATA_DIR:-/data}"
    mkdir -p "$data"
    chown subsyncer:subsyncer "$data" "$data"/subsyncer.db* 2>/dev/null || true
    # New files (backups, temp files) inherit a group-writable mask, like
    # most media containers.
    umask "${UMASK:-002}"
    exec gosu subsyncer subsyncer "$@"
fi
exec subsyncer "$@"
