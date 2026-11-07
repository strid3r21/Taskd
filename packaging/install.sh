#!/bin/sh
# Install Taskd from a release tarball. Default prefix: ~/.local (no root needed).
# Usage: ./install.sh [PREFIX]      e.g. sudo ./install.sh /usr/local
set -eu
PREFIX="${1:-$HOME/.local}"
HERE="$(cd "$(dirname "$0")" && pwd)"
install -Dm755 "$HERE/taskdm" "$PREFIX/bin/taskdm"
install -Dm644 "$HERE/taskd-monitor.desktop" "$PREFIX/share/applications/taskd-monitor.desktop"
install -Dm644 "$HERE/taskd-monitor.svg" "$PREFIX/share/icons/hicolor/scalable/apps/taskd-monitor.svg"
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$PREFIX/share/applications" || true
# If an icon cache exists there, GTK trusts it over the files, so a new icon stays invisible
# (the panel shows a generic icon) until the cache is rebuilt.
command -v gtk-update-icon-cache >/dev/null 2>&1 && gtk-update-icon-cache -q -f -t "$PREFIX/share/icons/hicolor" || true
echo "Installed Taskd: run it from your menu or with $PREFIX/bin/taskdm"
