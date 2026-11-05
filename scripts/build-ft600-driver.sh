#!/usr/bin/env bash
# Build libonidriver_ft600.so (the ONI FT600 driver, C) against FTDI's D3XX library.
#
# FTDI's D3XX library is proprietary and cannot be redistributed here. Download
# "D3XX Linux x86_64" from https://ftdichip.com/drivers/d3xx-drivers/ and extract it, then:
#
#   scripts/build-ft600-driver.sh /path/to/extracted/libftd3xx-linux-x86_64-*
#
# Installs to ~/.local/lib (found automatically by oers). Override with PREFIX=/usr/local.
set -euo pipefail
FTDI_DIR="${1:?usage: $0 <extracted FTDI D3XX directory>}"
HERE="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$HERE/crates/oni-sys/liboni"
BUILD="$HERE/target/ft600-driver"
PREFIX="${PREFIX:-$HOME/.local}"

STATIC="$(find "$FTDI_DIR" -name 'libftd3xx-static.a' | head -1)"
# Prefer the header shipped next to the library (packages also carry demo-app copies).
HDR="$(find "$(dirname "$STATIC")" -maxdepth 1 -iname 'ftd3xx.h' | head -1)"
[[ -n "$HDR" ]] || HDR="$(find "$FTDI_DIR" -iname 'ftd3xx.h' | head -1)"
[[ -f "$STATIC" ]] || { echo "libftd3xx-static.a not found under $FTDI_DIR" >&2; exit 1; }
[[ -f "$HDR" ]] || { echo "ftd3xx.h not found under $FTDI_DIR" >&2; exit 1; }

# D3XX 1.1.x (FTDI's C rewrite on libusb) breaks the ONI driver: a 0 ms pipe timeout now means
# "block forever", so oni_init_ctx hangs polling the control pipe. Require the 1.0.x line.
VER="$(find "$(dirname "$STATIC")" -maxdepth 1 -name 'libftd3xx.so.*.*.*' -printf '%f\n' | head -1 | sed 's/^libftd3xx\.so\.//')"
if [[ -z "$VER" ]]; then
    # No versioned .so: 1.0.x is C++ (device_lib classes); 1.1.x bundles libusb instead.
    if nm "$STATIC" 2>/dev/null | grep -q device_lib; then VER="1.0.x"; else VER="1.1.x"; fi
fi
echo "D3XX version: $VER"
if [[ "$VER" != 1.0.* && "${FORCE_D3XX:-0}" != 1 ]]; then
    cat >&2 <<EOF
D3XX $VER is not supported: with 1.1.x the driver hangs in oni_init_ctx (control-pipe read
with timeout 0 blocks forever). Use the 1.0.x line; 1.0.14 is tested:
  https://web.archive.org/web/20240519150808id_/https://ftdichip.com/wp-content/uploads/2023/06/libftd3xx-linux-x86_64-1.0.14.tgz
  sha256 ca71469fc2267ba655c0a9af9aa4cb853e4adad1dfd438df83f72d0b6b7ac265
Set FORCE_D3XX=1 to build anyway.
EOF
    exit 1
fi

rm -rf "$BUILD" && mkdir -p "$BUILD/include"
cp "$(dirname "$HDR")"/*.h "$BUILD/include/"
# The driver includes <FTD3XX.h>; FTDI's Linux package ships ftd3xx.h.
[[ -f "$BUILD/include/FTD3XX.h" ]] || ln -s "$(basename "$HDR")" "$BUILD/include/FTD3XX.h"

gcc -shared -fPIC -O2 -fno-omit-frame-pointer -pthread \
    -I"$BUILD/include" -I"$SRC" \
    "$SRC/drivers/ft600/onidriver_ft600.c" "$SRC/drivers/ft600/circbuffer.c" \
    "$STATIC" -lstdc++ -Wl,-soname,libonidriver_ft600.so \
    -o "$BUILD/libonidriver_ft600.so"

mkdir -p "$PREFIX/lib"
install -m 0755 "$BUILD/libonidriver_ft600.so" "$PREFIX/lib/libonidriver_ft600.so"
echo "Installed $PREFIX/lib/libonidriver_ft600.so"
echo "If you have not yet: sudo install -m 0644 $HERE/scripts/60-open-ephys-ft600.rules /etc/udev/rules.d/ && sudo udevadm control --reload && sudo udevadm trigger"
