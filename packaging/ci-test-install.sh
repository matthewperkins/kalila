#!/bin/sh
# Installs a kalila release on the distribution this runs on and checks that it runs. Used by
# .github/workflows/release.yml in a container per distribution; no board, no display.
#
#   packaging/ci-test-install.sh package DIST    # install the .deb or .rpm from DIST, test,
#                                                # remove, check nothing is left
#   packaging/ci-test-install.sh tarball TARBALL # unpack, install the runtime libraries (Alpine,
#                                                # Void, Arch),
#                                                # test from the folder
#
# The test: every symbol of both binaries resolves; the libraries kalila dlopen()s (Vulkan
# loader, Wayland, xkbcommon) are installed; kalila-headless acquires from the simulated
# board for 3 s with no samples lost.
set -eu
fail() {
    echo "ci-test-install: $*" >&2
    exit 1
}
[ $# -eq 2 ] || fail "usage: $0 package DIST | tarball TARBALL"
mode=$1
. /etc/os-release
echo "== $PRETTY_NAME"

have_lib() {
    # The dynamic loader's search, without ldconfig (musl has none).
    for d in /usr/lib64 /usr/lib /lib64 /lib /usr/lib/x86_64-linux-gnu /lib/x86_64-linux-gnu; do
        [ -e "$d/$1" ] && return 0
    done
    return 1
}

run_checks() {
    bindir=$1
    for b in "$bindir/kalila" "$bindir/kalila-headless"; do
        [ -x "$b" ] || fail "$b missing"
        if ldd --version 2>&1 | grep -qi musl; then
            # musl resolves every symbol at load; `ldd` loads the binary and lists them.
            ldd "$b" >/tmp/ldd.txt 2>&1 || { cat /tmp/ldd.txt; fail "$b does not load"; }
        else
            ldd -r "$b" >/tmp/ldd.txt 2>&1 || true
        fi
        cat /tmp/ldd.txt
        if grep -Eq "not found|undefined symbol|Error relocating" /tmp/ldd.txt; then
            fail "$b: unresolved libraries or symbols"
        fi
    done
    for l in libvulkan.so.1 libwayland-client.so.0 libwayland-cursor.so.0 libxkbcommon.so.0; do
        have_lib "$l" || fail "$l (loaded at run time) is not installed"
    done
    out=$(LD_BIND_NOW=1 "$bindir/kalila-headless" --seconds 3 --no-audio 2>&1) || {
        echo "$out"
        fail "kalila-headless failed"
    }
    echo "$out" | tail -3
    last=$(echo "$out" | grep '^t=' | tail -1)
    [ -n "$last" ] || fail "kalila-headless printed no status"
    echo "$last" | grep -q 'dropped=0 gaps=0 bad=0' || fail "kalila-headless lost samples: $last"
    echo "$last" | grep -q 'samples=[1-9]' || fail "kalila-headless acquired nothing: $last"
}

case "$mode" in
package)
    # Absolute: apt-get reads "dist/x.deb" as package "dist", release "x.deb".
    dist=$(cd "$2" && pwd -P)
    if command -v apt-get >/dev/null; then
        export DEBIAN_FRONTEND=noninteractive
        apt-get update -q
        apt-get install -y -q "$dist"/kalila_*_amd64.deb
        run_checks /usr/bin
        [ -f /usr/lib/udev/rules.d/60-open-ephys-ft600.rules ] || fail "udev rule missing"
        getent group realtime >/dev/null || fail "realtime group not created"
        apt-get remove -y -q kalila
        apt-get purge -y -q kalila
    elif command -v dnf >/dev/null; then
        dnf install -y "$dist"/kalila-*.x86_64.rpm
        run_checks /usr/bin
        [ -f /usr/lib/udev/rules.d/60-open-ephys-ft600.rules ] || fail "udev rule missing"
        getent group realtime >/dev/null || fail "realtime group not created"
        dnf remove -y kalila
    else
        fail "neither apt-get nor dnf"
    fi
    for f in /usr/bin/kalila /usr/bin/kalila-headless /usr/share/applications/kalila.desktop \
        /usr/lib/udev/rules.d/60-open-ephys-ft600.rules /usr/share/doc/kalila /usr/libexec/kalila; do
        [ ! -e "$f" ] || fail "$f left after removing the package"
    done
    ;;
tarball)
    tarball=$2
    if command -v apk >/dev/null; then
        apk add --no-cache alsa-lib pipewire-libs vulkan-loader wayland-libs-client \
            wayland-libs-cursor libxkbcommon libgcc
    elif command -v pacman >/dev/null; then
        pacman -Syu --noconfirm alsa-lib libpipewire vulkan-icd-loader wayland libxkbcommon gcc-libs
    elif command -v xbps-install >/dev/null; then
        xbps-install -Syu xbps
        # tar: Void's container image has none.
        xbps-install -Sy alsa-lib libpipewire vulkan-loader wayland libxkbcommon libgcc tar
    else
        fail "no known package manager for the runtime libraries"
    fi
    dir=$(mktemp -d)
    tar -xzf "$tarball" -C "$dir" --strip-components=1
    run_checks "$dir/bin"
    ;;
*) fail "unknown mode $mode" ;;
esac
echo "== $PRETTY_NAME: passed"
