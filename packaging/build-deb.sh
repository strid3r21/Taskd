#!/bin/sh
# Build a .deb from target/release/taskd. Usage: packaging/build-deb.sh [OUT_DIR]
# Uses dpkg-deb when available, otherwise assembles the archive with ar + tar.
set -eu
cd "$(dirname "$0")/.."
OUT="${1:-dist}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
ROOT="$(mktemp -d)"
chmod 755 "$ROOT"
trap 'rm -rf "$ROOT"' EXIT

install -Dm755 target/release/taskd "$ROOT/usr/bin/taskd"
install -Dm644 packaging/taskd.desktop "$ROOT/usr/share/applications/taskd.desktop"
install -Dm644 packaging/taskd.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/taskd.svg"
install -Dm644 LICENSE "$ROOT/usr/share/doc/taskd/copyright"
SIZE="$(du -sk "$ROOT/usr" | cut -f1)"

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<CONTROL
Package: taskd
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE
Depends: libc6, libgl1
Recommends: policykit-1 | pkexec
Maintainer: strid3r21 <strid3r21@users.noreply.github.com>
Description: Fast, lightweight task manager for Linux
 Processes, performance graphs (CPU, memory, disk, network, GPU),
 users, systemd services and startup applications in a single small binary.
CONTROL

mkdir -p "$OUT"
DEB="$OUT/taskd_${VERSION}_${ARCH}.deb"
if command -v dpkg-deb >/dev/null 2>&1; then
    dpkg-deb --root-owner-group --build "$ROOT" "$DEB" >/dev/null
else
    W="$(mktemp -d)"
    echo "2.0" > "$W/debian-binary"
    tar -C "$ROOT/DEBIAN" --owner=0 --group=0 -czf "$W/control.tar.gz" .
    tar -C "$ROOT" --owner=0 --group=0 --exclude=./DEBIAN -czf "$W/data.tar.gz" .
    rm -f "$DEB"
    (cd "$W" && ar rc "$OLDPWD/$DEB" debian-binary control.tar.gz data.tar.gz)
    rm -rf "$W"
fi
echo "$DEB"
