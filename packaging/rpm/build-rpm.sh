#!/bin/bash
# Builds target/rpm/salamander-rs-<version>-1.x86_64.rpm for AlmaLinux / RHEL 9 and 10.
#
# Requirements: cargo with the GTK 4 development files, a static libsodium (libsodium.a),
# and rpmbuild. The binary must be built against glibc <= 2.34 symbols in practice; Ubuntu
# 24.04 and AlmaLinux 9 qualify (newer glibc symbols used by Rust's std are weak).
#
#   SODIUM_STATIC_DIR  directory with libsodium.a (default: first of /usr/lib64,
#                      /usr/lib/x86_64-linux-gnu that contains it)
#   RPMBUILD           rpmbuild command (default: rpmbuild)
#   CARGO_FLAGS        extra cargo flags, e.g. --offline
set -euo pipefail
cd "$(dirname "$0")/../.."

version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
sodium=${SODIUM_STATIC_DIR:-}
if [ -z "$sodium" ]; then
    for dir in /usr/lib64 /usr/lib/x86_64-linux-gnu; do
        [ -f "$dir/libsodium.a" ] && sodium=$dir && break
    done
fi
[ -f "$sodium/libsodium.a" ] || { echo "libsodium.a nenalezena; nastavte SODIUM_STATIC_DIR" >&2; exit 1; }

target=${CARGO_TARGET_DIR:-target}/rpm-build
# shellcheck disable=SC2086
SALAMANDER_SODIUM_STATIC=$sodium CARGO_TARGET_DIR=$target cargo build --release ${CARGO_FLAGS:-}
binary=$target/release/salamander-rs
if readelf -d "$binary" | grep -q 'libsodium'; then
    echo "libsodium není přilinkována staticky" >&2
    exit 1
fi

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
mkdir -p "$work/SOURCES"
strip -o "$work/SOURCES/salamander-rs" "$binary"
cp assets/cz.k4.salamander-rs.desktop assets/salamander-rs.svg LICENSE README.md "$work/SOURCES/"
${RPMBUILD:-rpmbuild} -bb \
    --define "_topdir $work" \
    --define "pkg_version $version" \
    packaging/rpm/salamander-rs.spec
mkdir -p target/rpm
cp "$work"/RPMS/x86_64/*.rpm target/rpm/
ls -l target/rpm/salamander-rs-"$version"-*.rpm
