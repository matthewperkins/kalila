#!/bin/sh
# Builds kalila.app for macOS (Apple silicon), checks it, and writes kalila-macos-arm64.zip
# into OUT_DIR. Run by the workflows on a macOS runner; works by hand on any Mac with Rust
# and the Command Line Tools:
#
#   packaging/make-macos-app.sh OUT_DIR
#
# The bundle: Contents/MacOS/kalila (the app) and kalila-headless (acquisition and recording
# without windows), Info.plist, the icon, and the licences of everything compiled in. It is
# ad-hoc signed (Apple silicon runs no unsigned code; an ad-hoc signature names nobody, so a
# browser download still meets Gatekeeper: see the README). Zipped with ditto, which keeps
# the signature intact. The zip's name has no version, so
# releases/latest/download/kalila-macos-arm64.zip is always the newest.
#
# The version is the workspace's (Cargo.toml). With KALILA_TAG set the tag must be
# v<version>; without it the short version gets a suffix, <version>-dev.g<commit>.
# Never touches a board: the checks run the simulator.
set -eu
fail() {
    echo "make-macos-app: $*" >&2
    exit 1
}
[ $# -eq 1 ] || fail "usage: $0 OUT_DIR"
[ "$(uname -s)" = Darwin ] || fail "macOS only"
[ "$(uname -m)" = arm64 ] || fail "Apple silicon only"
root=$(cd "$(dirname "$0")/.." && pwd -P)
mkdir -p "$1"
out=$(cd "$1" && pwd -P)
cd "$root"

version=$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml)
[ -n "$version" ] || fail "no version in Cargo.toml"
if [ -n "${KALILA_TAG:-}" ]; then
    [ "$KALILA_TAG" = "v$version" ] || fail "the tag is $KALILA_TAG but Cargo.toml says $version"
    short=$version
else
    short="$version-dev.g$(git rev-parse --short=9 HEAD)"
fi
commit=$(git describe --always --dirty --tags 2>/dev/null || git rev-parse HEAD)

cargo build --release --locked -p kalila
cargo build --release --locked -p engine --example headless

app="$root/target/release-dist/kalila.app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources/licenses"
install -m 0755 target/release/kalila "$app/Contents/MacOS/kalila"
install -m 0755 target/release/examples/headless "$app/Contents/MacOS/kalila-headless"
strip "$app/Contents/MacOS/kalila" "$app/Contents/MacOS/kalila-headless"

# Nothing outside the system may be linked: the app must run on a Mac with nothing installed.
for b in "$app"/Contents/MacOS/*; do
    if otool -L "$b" | tail -n +2 | grep -v -E '^[[:space:]]*(/usr/lib/|/System/Library/)'; then
        fail "$(basename "$b") links a library outside the system (above)"
    fi
done

# The oldest macOS the binaries were built for (LC_BUILD_VERSION), the newer of the two.
minos=$(for b in "$app"/Contents/MacOS/*; do
    otool -l "$b" | awk '/LC_BUILD_VERSION/ {f = 1} f && $1 == "minos" {print $2; exit}'
done | sort -t. -k1,1n -k2,2n | tail -1)
[ -n "$minos" ] || fail "no minos in the binaries"
sed -e "s/SHORTVERSION/$short/" -e "s/VERSION/$version/" -e "s/MINOS/$minos/" \
    packaging/macos/Info.plist >"$app/Contents/Info.plist"
plutil -lint "$app/Contents/Info.plist"

# The icon, from the 1024 px render (packaging/icon/ICON-CREDITS.md says how it is made).
set_dir=$(mktemp -d)/kalila.iconset
mkdir -p "$set_dir"
for s in 16 32 128 256 512; do
    sips -z $s $s packaging/icon/kalila-1024.png --out "$set_dir/icon_${s}x${s}.png" >/dev/null
    sips -z $((s * 2)) $((s * 2)) packaging/icon/kalila-1024.png --out "$set_dir/icon_${s}x${s}@2x.png" >/dev/null
done
iconutil -c icns "$set_dir" -o "$app/Contents/Resources/kalila.icns"

install -m 0644 LICENSE "$app/Contents/Resources/licenses/kalila.LICENSE"
install -m 0644 crates/oni-sys/liboni/LICENSE "$app/Contents/Resources/licenses/liboni.LICENSE"
python3 packaging/collect-licenses.py aarch64-apple-darwin "$app/Contents/Resources/licenses/crates"
install -m 0644 README.md "$app/Contents/Resources/README.md"
printf '%s\n' "$commit" >"$app/Contents/Resources/COMMIT"
{
    echo "kalila $short, macOS arm64, for macOS $minos or newer"
    echo "built $(date -u +%Y-%m-%dT%H:%M:%SZ) on macOS $(sw_vers -productVersion)"
    rustc --version
    echo
    echo "dynamic libraries:"
    for b in "$app"/Contents/MacOS/*; do
        echo "$(basename "$b"):"
        otool -L "$b" | tail -n +2 | awk '{print "  " $1}'
    done
} >"$app/Contents/Resources/BUILD-INFO"
cat "$app/Contents/Resources/BUILD-INFO"

codesign --force -s - "$app/Contents/MacOS/kalila-headless"
codesign --force -s - "$app"
codesign --verify --strict --verbose=2 "$app"

# The bundle's own programs run: the app answers --help without opening a window, and
# kalila-headless acquires from the simulated board with nothing lost.
help=$("$app/Contents/MacOS/kalila" --help) || fail "kalila --help failed"
echo "$help" | grep -q -- '--sim' || fail "kalila --help did not answer: $help"
status=$("$app/Contents/MacOS/kalila-headless" --seconds 3 --no-audio 2>&1) || {
    echo "$status" >&2
    fail "kalila-headless failed"
}
last=$(echo "$status" | grep 'samples=' | tail -1)
[ -n "$last" ] || fail "kalila-headless printed no status"
echo "$last" | grep -q 'dropped=0 gaps=0 bad=0' || fail "kalila-headless lost samples: $last"
echo "$last" | grep -q 'samples=[1-9]' || fail "kalila-headless acquired nothing: $last"

# Without extended attributes: ditto would store them as "._name" entries, which `unzip`
# writes into the bundle as files, and the signature then no longer matches ("a sealed
# resource is missing or invalid"). Nothing in the bundle needs one.
rm -f "$out/kalila-macos-arm64.zip"
(cd "$root/target/release-dist" && ditto -c -k --norsrc --noextattr --keepParent kalila.app "$out/kalila-macos-arm64.zip")

# As a user installs it: unzip (not ditto) somewhere else; the signature must still hold
# and the programs still run from there.
try=$(mktemp -d)
(cd "$try" && unzip -q "$out/kalila-macos-arm64.zip")
[ "$(ls -A "$try")" = kalila.app ] || fail "the zip holds more than kalila.app: $(ls -A "$try")"
codesign --verify --strict --verbose=2 "$try/kalila.app" || fail "the unzipped app's signature does not verify"
"$try/kalila.app/Contents/MacOS/kalila" --help >/dev/null || fail "the unzipped kalila --help failed"
rm -rf "$try"
ls -l "$out/kalila-macos-arm64.zip"
