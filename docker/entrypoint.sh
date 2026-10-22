#!/bin/sh
# Drops privileges to PUID:PGID, then execs the binary so it is PID 1 and
# receives SIGTERM directly.
#
# The media volumes sit on an NFS export with `all_squash`: the uid the process
# runs as is what the NAS sees, hence PUID/PGID. Only the state directory is
# ever chown-ed — never the media volumes, which would be a recursive rewrite of
# the whole NAS over NFS.
set -eu

PUID="${PUID:-1000}"
PGID="${PGID:-1000}"
STATE_DIR="${MO_STATE_DIR:-/var/lib/media-organizer}"

# A bare flag or one of our subcommands means "run media-organizer"; anything
# else (e.g. `sh`) is executed as given, which helps debugging.
case "${1:-}" in
    ""|-*|validate-config|healthcheck|recover) set -- media-organizer "$@" ;;
esac

# Started with `user:` (already unprivileged): nothing to remap.
if [ "$(id -u)" -ne 0 ]; then
    exec "$@"
fi

mkdir -p "$STATE_DIR"
chown "$PUID:$PGID" "$STATE_DIR"
exec setpriv --reuid="$PUID" --regid="$PGID" --clear-groups "$@"
