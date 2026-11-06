#!/bin/sh
set -eu
# Keep the complete extracted directory together; never point a symlink elsewhere.
base=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd -P)
name=${0##*/}
case "$name" in
  vaultsyncd-server)
    config=${VAULTSYNCD_CONFIG-/etc/vaultsyncd/vaultsyncd.toml}
    case "$#:${1-}" in 1:--help|1:-h) exec "$base/lib/@LOADER@" --library-path "$base/lib" "$base/libexec/$name.elf" "$@";; esac
    if [ "$#" = 2 ] && [ "$1" = --config ]; then config=$2
    elif [ "$#" != 0 ]; then echo 'usage: vaultsyncd-server [--config PATH]' >&2; exit 2; fi
    python3 "$base/share/vaultsyncd/preflight.py" "$(command -v findmnt)" --toml "$config"
    VAULTSYNCD_WEB_DIR=${VAULTSYNCD_WEB_DIR-"$base/share/vaultsyncd/web"}
    export VAULTSYNCD_WEB_DIR
    ;;
  vaultsyncd-admin)
    if [ "${1-}" = init ]; then
      state=/var/lib/vaultsyncd
      previous=
      for argument in "$@"; do
        if [ "$previous" = --state-dir ]; then state=$argument; fi
        previous=$argument
      done
      python3 "$base/share/vaultsyncd/preflight.py" "$(command -v findmnt)" --state "$state"
    fi
    ;;
  *) echo 'unknown portable program' >&2; exit 2;;
esac
exec "$base/lib/@LOADER@" --library-path "$base/lib" "$base/libexec/$name.elf" "$@"
