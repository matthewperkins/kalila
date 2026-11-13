#!/bin/sh
# Installs an kalila launcher into ~/.local/share/applications pointing at this checkout.
# Build first: cargo build --release
set -e
root=$(cd "$(dirname "$0")/.." && pwd)
apps=${XDG_DATA_HOME:-$HOME/.local/share}/applications
mkdir -p "$apps"
# Install icons into the hicolor theme so launchers that only resolve icon
# names (e.g. hyprlauncher/hyprtoolkit) can find them, not just file paths.
icons=${XDG_DATA_HOME:-$HOME/.local/share}/icons/hicolor
for sz in 32 48 64 128 256; do
    mkdir -p "$icons/${sz}x${sz}/apps"
    cp "$root/packaging/icon/kalila-$sz.png" "$icons/${sz}x${sz}/apps/kalila.png"
done
# hyprtoolkit ignores theme dirs without an index.theme
[ -f "$icons/index.theme" ] || cp /usr/share/icons/hicolor/index.theme "$icons/" 2>/dev/null || true
command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -qtf "$icons" || true
cat >"$apps/kalila.desktop" <<DESK
[Desktop Entry]
Type=Application
Name=kalila
GenericName=Open Ephys acquisition (Rust)
Comment=Acquire and record from the Open Ephys ONI acquisition board
Exec=$root/packaging/kalila-launch.sh
Icon=kalila
Terminal=false
Categories=Science;
StartupWMClass=kalila
StartupNotify=false
Actions=Simulator;

[Desktop Action Simulator]
Name=kalila (simulated board)
Exec=$root/packaging/kalila-launch.sh --sim
DESK
command -v update-desktop-database >/dev/null && update-desktop-database -q "$apps" || true
echo "installed $apps/kalila.desktop"
