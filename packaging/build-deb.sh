#!/bin/sh
# Build a .deb. Usage: [TASKD_BIN=path/to/taskdm] packaging/build-deb.sh [OUT_DIR]
# Uses dpkg-deb when available, otherwise assembles the archive with ar + tar.
set -eu
cd "$(dirname "$0")/.."
OUT="${1:-dist}"
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
ARCH="$(dpkg --print-architecture 2>/dev/null || echo amd64)"
ROOT="$(mktemp -d)"
chmod 755 "$ROOT"
trap 'rm -rf "$ROOT"' EXIT

install -Dm755 "${TASKD_BIN:-target/release/taskdm}" "$ROOT/usr/bin/taskdm"
install -Dm644 packaging/taskd-monitor.desktop "$ROOT/usr/share/applications/taskd-monitor.desktop"
install -Dm644 packaging/taskd-monitor.svg "$ROOT/usr/share/icons/hicolor/scalable/apps/taskd-monitor.svg"
install -Dm644 LICENSE "$ROOT/usr/share/doc/taskd-monitor/copyright"
install -Dm755 packaging/install-update "$ROOT/usr/libexec/taskd-monitor/install-update"
install -Dm644 packaging/io.github.strid3r21.taskd.policy "$ROOT/usr/share/polkit-1/actions/io.github.strid3r21.taskd.policy"
install -Dm644 packaging/release-key.pem "$ROOT/usr/share/taskd-monitor/release-key.pem"
# Optional privileged helper (per-app network): shipped, but only enabled from Taskd's settings.
install -Dm644 packaging/taskd-helper.service "$ROOT/usr/lib/systemd/system/taskd-helper.service"
SIZE="$(du -sk "$ROOT/usr" | cut -f1)"

mkdir -p "$ROOT/DEBIAN"
cat > "$ROOT/DEBIAN/control" <<CONTROL
Package: taskd-monitor
Version: $VERSION
Section: admin
Priority: optional
Architecture: $ARCH
Installed-Size: $SIZE
Depends: libc6 (>= 2.31), libgl1, openssl
Recommends: pkexec | policykit-1
Maintainer: strid3r21 <strid3r21@users.noreply.github.com>
Description: Fast, lightweight task manager for Linux
 Processes, performance graphs (CPU, memory, disk, network, GPU),
 users, systemd services and startup applications in a single small binary.
CONTROL

cat > "$ROOT/DEBIAN/postinst" <<'POSTINST'
#!/bin/sh
set -e
if [ -d /run/systemd/system ]; then
    systemctl daemon-reload || true
    # Pick up the new binary if the helper is in use.
    systemctl try-restart taskd-helper.service || true
fi
POSTINST
cat > "$ROOT/DEBIAN/prerm" <<'PRERM'
#!/bin/sh
set -e
if [ "$1" = remove ] && [ -d /run/systemd/system ]; then
    systemctl disable --now taskd-helper.service || true
fi
PRERM
chmod 755 "$ROOT/DEBIAN/postinst" "$ROOT/DEBIAN/prerm"

mkdir -p "$OUT"
DEB="$OUT/taskd-monitor_${VERSION}_${ARCH}.deb"
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
