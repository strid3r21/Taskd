#!/bin/sh
# Install Taskd from a release tarball. Default prefix: ~/.local (no root needed).
# Usage: ./install.sh [PREFIX]      e.g. sudo ./install.sh /usr/local
set -eu
PREFIX="${1:-$HOME/.local}"
HERE="$(cd "$(dirname "$0")" && pwd)"
install -Dm755 "$HERE/taskd" "$PREFIX/bin/taskd"
install -Dm644 "$HERE/taskd.desktop" "$PREFIX/share/applications/taskd.desktop"
install -Dm644 "$HERE/taskd.svg" "$PREFIX/share/icons/hicolor/scalable/apps/taskd.svg"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$PREFIX/share/applications" || true
echo "Installed taskd to $PREFIX/bin/taskd"
