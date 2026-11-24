#!/bin/sh
# Launcher used by the installed kalila.desktop: runs kalila and keeps its log, since a
# desktop launch has no terminal. Log: ~/.local/state/kalila/kalila.log (previous run kept as
# kalila.log.1). From a tarball, kalila is found next to this script's ../bin, else on PATH.
state=${XDG_STATE_HOME:-$HOME/.local/state}/kalila
mkdir -p "$state"
[ -f "$state/kalila.log" ] && mv -f "$state/kalila.log" "$state/kalila.log.1"
here=$(dirname "$(readlink -f "$0")")
bin=kalila
[ -x "$here/../bin/kalila" ] && bin="$here/../bin/kalila"
exec "$bin" "$@" >"$state/kalila.log" 2>&1
