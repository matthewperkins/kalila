#!/bin/sh
# Launcher used by oers.desktop: runs the release build and keeps its log, since a desktop
# launch has no terminal. Log: ~/.local/state/oers/oers.log (previous run kept as oers.log.1).
dir=$(dirname "$(readlink -f "$0")")/..
state=${XDG_STATE_HOME:-$HOME/.local/state}/oers
mkdir -p "$state"
[ -f "$state/oers.log" ] && mv -f "$state/oers.log" "$state/oers.log.1"
exec "$dir/target/release/oers" "$@" >"$state/oers.log" 2>&1
