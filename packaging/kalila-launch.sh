#!/bin/sh
# Launcher used by kalila.desktop: runs the release build and keeps its log, since a desktop
# launch has no terminal. Log: ~/.local/state/kalila/kalila.log (previous run kept as kalila.log.1).
dir=$(dirname "$(readlink -f "$0")")/..
state=${XDG_STATE_HOME:-$HOME/.local/state}/kalila
mkdir -p "$state"
[ -f "$state/kalila.log" ] && mv -f "$state/kalila.log" "$state/kalila.log.1"
exec "$dir/target/release/kalila" "$@" >"$state/kalila.log" 2>&1
