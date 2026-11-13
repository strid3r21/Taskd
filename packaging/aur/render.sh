#!/bin/sh
# Fill in PKGBUILD.in with a release's version and tarball checksums.
# Usage: packaging/aur/render.sh DIST_DIR VERSION   (writes DIST_DIR/PKGBUILD)
set -eu
dist="$1"
v="$2"
here="$(dirname "$0")"
sum() { sha256sum "$dist/taskd-monitor-$v-$1-linux.tar.gz" | cut -d' ' -f1; }
sed -e "s/@VERSION@/$v/" -e "s/@SHA_X86_64@/$(sum x86_64)/" -e "s/@SHA_AARCH64@/$(sum aarch64)/" \
    "$here/PKGBUILD.in" > "$dist/PKGBUILD"
! grep -q '@[A-Z_0-9]*@' "$dist/PKGBUILD" || { echo "unfilled placeholder in PKGBUILD" >&2; exit 1; }
