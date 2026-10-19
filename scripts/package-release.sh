#!/usr/bin/env bash
# Build a release of Scion for this machine and archive it for a GitHub release.
#
#   ./scripts/package-release.sh v0.1.0                  # platform guessed from this machine
#   ./scripts/package-release.sh v0.1.0 macos-arm64      # -> dist/Scion-v0.1.0-macos-arm64.zip
#   ./scripts/package-release.sh v0.1.0 linux-x86_64     # -> dist/Scion-v0.1.0-linux-x86_64.tar.gz
#   ./scripts/package-release.sh v0.1.0 windows-x86_64   # -> dist/Scion-v0.1.0-windows-x86_64.zip
#
# The archive holds scion-desktop, scn, scion-server and scion-import plus README.md.
# The platform must match the machine: this script doesn't cross-compile.
set -euo pipefail

cd "$(dirname "$0")/.."

usage() {
    echo "usage: $0 <version tag, e.g. v0.1.0> [macos-arm64|macos-x86_64|linux-x86_64|windows-x86_64]" >&2
    exit 2
}

# The platform cargo builds for, from rustc's host triple (not uname, which a Rosetta shell fakes).
detect() {
    case "$(rustc -vV | sed -n 's/^host: //p')" in
        aarch64-apple-darwin) echo macos-arm64 ;;
        x86_64-apple-darwin) echo macos-x86_64 ;;
        x86_64-unknown-linux-*) echo linux-x86_64 ;;
        x86_64-pc-windows-*) echo windows-x86_64 ;;
        *) echo unknown ;;
    esac
}

version="${1:-}"
platform="${2:-$(detect)}"
[ -n "$version" ] || usage
case "$platform" in
    macos-arm64 | macos-x86_64 | linux-x86_64 | windows-x86_64) ;;
    *) usage ;;
esac

# Refuse to package for another platform than this one.
if [ "$platform" != "$(detect)" ]; then
    echo "this machine is $(detect), not $platform" >&2
    exit 1
fi

bins=(scion-desktop scn scion-server scion-import)
exe=""
[ "${platform%%-*}" = windows ] && exe=".exe"

args=()
for b in "${bins[@]}"; do
    args+=(--bin "$b")
done
cargo build --release --locked "${args[@]}"

name="Scion-$version-$platform"
rm -rf "dist/$name" "dist/$name".*
mkdir -p "dist/$name"
for b in "${bins[@]}"; do
    cp "target/release/$b$exe" "dist/$name/"
done
cp README.md "dist/$name/"
[ -f LICENSE ] && cp LICENSE "dist/$name/"

case "$platform" in
    # ditto keeps macOS metadata and permissions the way Finder's "Compress" does.
    macos-*) ditto -c -k --keepParent "dist/$name" "dist/$name.zip" && echo "dist/$name.zip" ;;
    linux-*) tar -C dist -czf "dist/$name.tar.gz" "$name" && echo "dist/$name.tar.gz" ;;
    # 7-Zip is preinstalled on GitHub's Windows runners.
    windows-*) (cd dist && 7z a -tzip -bso0 "$name.zip" "$name") && echo "dist/$name.zip" ;;
esac
