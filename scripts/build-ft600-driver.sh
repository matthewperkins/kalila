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
HDR="$(find "$FTDI_DIR" -iname 'ftd3xx.h' | head -1)"
[[ -f "$STATIC" ]] || { echo "libftd3xx-static.a not found under $FTDI_DIR" >&2; exit 1; }
[[ -f "$HDR" ]] || { echo "ftd3xx.h not found under $FTDI_DIR" >&2; exit 1; }

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
