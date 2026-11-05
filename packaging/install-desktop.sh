#!/bin/sh
# Installs an oers launcher into ~/.local/share/applications pointing at this checkout.
# Build first: cargo build --release
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
mkdir -p "$apps"
cat >"$apps/oers.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=oers
GenericName=Open Ephys acquisition (Rust)
Comment=Acquire and record from the Open Ephys ONI acquisition board
Exec=$root/packaging/oers-launch.sh
Icon=$root/packaging/icon/oers-256.png
Terminal=false
Categories=Science;
StartupWMClass=oers
StartupNotify=false
Actions=Simulator;

[Desktop Action Simulator]
Name=oers (simulated board)
Exec=$root/packaging/oers-launch.sh --sim
DESK
command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps" || true
echo "installed $apps/oers.desktop"
