#!/bin/sh
# Builds kalila's release for one C library, tests it, and writes its tarball (and, for gnu,
# the .deb and .rpm) into OUT_DIR. Run by .github/workflows/release.yml in a clean container;
# works anywhere the build dependencies below are installed.
#
#   packaging/ci-build.sh gnu  OUT_DIR    # on Rocky Linux 9: glibc 2.34, so the binaries run
#                                         # on any glibc >= 2.34 (RHEL/Alma/Rocky 9, Fedora,
#                                         # Debian 12+, Ubuntu 24.04+)
#                                         # with PipeWire >= 0.3.50 (so not Ubuntu 22.04)
#   packaging/ci-build.sh musl OUT_DIR    # on Alpine: dynamically linked against musl
#
# The version is the workspace's (Cargo.toml). With KALILA_TAG set (the workflow sets it for
# a pushed tag) the tag must be v<version>, or this fails. Without it the build is not a
# release and the version gets a pre-release suffix, <version>-dev.g<commit>.
# Never touches a board: there is none in CI.
set -eu
fail() {
    echo "ci-build: $*" >&2
    exit 1
}
[ $# -eq 2 ] || fail "usage: $0 gnu|musl OUT_DIR"
libc=$1
case "$libc" in gnu | musl) ;; *) fail "unknown C library $libc" ;; esac
root=$(cd "$(dirname "$0")/.." && pwd -P)
mkdir -p "$2"
out=$(cd "$2" && pwd -P)
cd "$root"

RUST_VERSION=1.95.0

# ---------------------------------------------------------------- build dependencies
if [ "$libc" = gnu ]; then
    # CRB has pipewire-devel; clang for bindgen (pipewire's bindings).
    dnf install -y dnf-plugins-core
    dnf config-manager --set-enabled crb
    dnf install -y gcc gcc-c++ make pkgconf-pkg-config git python3 \
        alsa-lib-devel pipewire-devel clang-devel xz
else
    apk add --no-cache build-base pkgconf git python3 curl bash \
        alsa-lib-dev pipewire-dev clang-dev linux-headers xz
fi
if ! command -v cargo >/dev/null; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs |
        sh -s -- -y --profile minimal --default-toolchain "$RUST_VERSION"
fi
. "$HOME/.cargo/env"
rustc --version

# musl: link dynamically, so that the Vulkan, Wayland and X11 libraries can be dlopen()ed.
if [ "$libc" = musl ]; then
    export RUSTFLAGS="-C target-feature=-crt-static"
fi

# ---------------------------------------------------------------- version
version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$version" ] || fail "no version in Cargo.toml"
if [ -n "${KALILA_TAG:-}" ]; then
    [ "$KALILA_TAG" = "v$version" ] || fail "the tag is $KALILA_TAG but Cargo.toml says $version"
    pkg_version=$version
else
    pkg_version="$version-dev.g$(git rev-parse --short=9 HEAD)"
fi
commit=$(git describe --always --dirty --tags 2>/dev/null || git rev-parse HEAD)

# ---------------------------------------------------------------- build and test
cargo build --release --locked -p kalila
cargo build --release --locked -p engine --example headless
# The deterministic simulation test and every unit test; nothing here opens a board (the
# FT600 driver's test checks that none is attached and only exercises the failure path).
cargo test --release --locked --workspace

# ---------------------------------------------------------------- release folder
name="kalila-$pkg_version-x86_64-linux-$libc"
rel="$root/target/release-dist/$name"
rm -rf "$rel"
mkdir -p "$rel/bin" "$rel/share" "$rel/licenses"
install -m 0755 target/release/kalila "$rel/bin/kalila"
install -m 0755 target/release/examples/headless "$rel/bin/kalila-headless"
strip "$rel/bin/kalila" "$rel/bin/kalila-headless"
install -m 0644 scripts/60-open-ephys-ft600.rules scripts/99-kalila-realtime.conf "$rel/share/"
install -m 0644 packaging/kalila.desktop "$rel/share/"
install -m 0755 packaging/kalila-desktop-launch.sh "$rel/share/"
cp -r packaging/icon "$rel/share/icon"
install -m 0644 README.md "$rel/README.md"
install -m 0644 packaging/INSTALL-tarball.md "$rel/INSTALL.md"
install -m 0644 LICENSE "$rel/licenses/kalila.LICENSE"
install -m 0644 crates/oni-sys/liboni/LICENSE "$rel/licenses/liboni.LICENSE"
target=x86_64-unknown-linux-$libc
python3 packaging/collect-licenses.py "$target" "$rel/licenses/crates"
printf '%s\n' "$commit" >"$rel/COMMIT"
{
    echo "kalila $pkg_version, x86_64-linux-$libc"
    echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ) on $(. /etc/os-release && echo "$PRETTY_NAME")"
    rustc --version
    echo
    echo "dynamic libraries (linked; Vulkan, Wayland, X11 and xkbcommon are loaded at run time):"
    for b in "$rel"/bin/*; do
        echo "$(basename "$b"):"
        objdump -p "$b" | sed -n 's/^ *NEEDED */  /p'
    done
    if [ "$libc" = gnu ]; then
        echo
        echo "newest glibc symbol version used:"
        objdump -T "$rel"/bin/* | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1 | sed 's/^/  /'
    fi
} >"$rel/BUILD-INFO"
cat "$rel/BUILD-INFO"

# A glibc build must not need more than 2.34 (the oldest supported distribution, RHEL 9).
if [ "$libc" = gnu ]; then
    newest=$(objdump -T "$rel"/bin/* | grep -o 'GLIBC_[0-9.]*' | sort -Vu | tail -1)
    [ "$(printf '%s\n' "$newest" GLIBC_2.34 | sort -V | tail -1)" = GLIBC_2.34 ] ||
        fail "the binaries need $newest, newer than GLIBC_2.34"
fi

tar -C "$root/target/release-dist" -czf "$out/$name.tar.gz" "$name"
echo "wrote $out/$name.tar.gz"

# ---------------------------------------------------------------- .deb and .rpm
if [ "$libc" = gnu ]; then
    NFPM_VERSION=2.48.0
    NFPM_SHA256=6d7f9a28b2f397934403950fcbba83f598b08d02edbe79a0815b3b4897341750
    if ! command -v nfpm >/dev/null; then
        t=$(mktemp -d)
        curl --proto '=https' -sSfL -o "$t/nfpm.tgz" \
            "https://github.com/goreleaser/nfpm/releases/download/v$NFPM_VERSION/nfpm_${NFPM_VERSION}_Linux_x86_64.tar.gz"
        echo "$NFPM_SHA256  $t/nfpm.tgz" | sha256sum -c -
        tar -xzf "$t/nfpm.tgz" -C "$t" nfpm
        install -m 0755 "$t/nfpm" /usr/local/bin/nfpm
    fi
    cp packaging/nfpm.yaml packaging/package-postinstall.sh packaging/package-postremove.sh "$rel/"
    (
        cd "$rel"
        export KALILA_VERSION="$pkg_version"
        nfpm package --config nfpm.yaml --packager deb --target "$out/"
        nfpm package --config nfpm.yaml --packager rpm --target "$out/"
    )
    rm "$rel/nfpm.yaml" "$rel/package-postinstall.sh" "$rel/package-postremove.sh"
fi
ls -l "$out"
