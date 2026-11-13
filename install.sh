#!/bin/sh
# Install the latest Taskd release into ~/.local (no root needed):
#
#   curl -fsSL https://raw.githubusercontent.com/strid3r21/Taskd/main/install.sh | sh
#
# Downloads the release tarball, checks it against the signed SHA256SUMS, then runs the
# tarball's own install.sh. Running it again updates to the latest release.
set -eu

BASE=https://github.com/strid3r21/Taskd/releases/latest/download
# Same key as packaging/release-key.pem and the one built into the app's updater.
PUBLIC_KEY='-----BEGIN PUBLIC KEY-----
MCowBQYDK2VwAyEAPdq9I/AcQb/ATfin0dq5nxh8lWHvb99kuPkk7wS6jAs=
-----END PUBLIC KEY-----'

fail() {
    echo "Taskd install failed: $1" >&2
    exit 1
}

fetch() {
    curl -fsSL --proto '=https' --tlsv1.2 -o "$2" "$BASE/$1" || fail "could not download $1"
}

# Everything runs from here, so a download cut off midway through can't run half a script.
main() {
    [ "$(uname -s)" = Linux ] || fail "Taskd only runs on Linux"
    arch=$(uname -m)
    case "$arch" in
        x86_64 | aarch64) ;;
        arm64) arch=aarch64 ;;
        *) fail "there are no release builds for $arch; see 'Building from source' in the README" ;;
    esac
    for tool in curl tar sha256sum mktemp; do
        command -v "$tool" >/dev/null 2>&1 || fail "$tool is required"
    done

    dir=$(mktemp -d)
    trap 'rm -rf "$dir"' EXIT
    trap 'exit 1' HUP INT PIPE TERM # dash skips the EXIT trap when a signal kills it
    fetch SHA256SUMS "$dir/SHA256SUMS"
    fetch SHA256SUMS.sig "$dir/SHA256SUMS.sig"

    # Ed25519 signatures need OpenSSL 3. Without it the checksum still catches a corrupt
    # download, but not a tampered release, so say so instead of staying quiet.
    case "$(openssl pkeyutl -help 2>&1 || true)" in
        *rawin*)
            printf '%s\n' "$PUBLIC_KEY" >"$dir/key.pem"
            openssl pkeyutl -verify -pubin -inkey "$dir/key.pem" -rawin \
                -in "$dir/SHA256SUMS" -sigfile "$dir/SHA256SUMS.sig" >/dev/null 2>&1 \
                || fail "the release signature is not valid; nothing was installed"
            echo "Signature OK"
            ;;
        *) echo "Warning: OpenSSL 3 not found, so the release signature was not checked" >&2 ;;
    esac

    name=$(awk -v a="$arch" '{f=$2; sub(/^\*?(\.\/)?/, "", f)} f ~ ("^taskd-monitor-[0-9.]+-" a "-linux\\.tar\\.gz$") {print f; exit}' "$dir/SHA256SUMS")
    [ -n "$name" ] || fail "no $arch Linux tarball is listed in this release"
    expected=$(awk -v n="$name" '{f=$2; sub(/^\*?(\.\/)?/, "", f)} f == n {print $1; exit}' "$dir/SHA256SUMS")

    echo "Downloading $name"
    fetch "$name" "$dir/$name"
    echo "$expected  $dir/$name" | sha256sum -c --quiet - >/dev/null 2>&1 \
        || fail "checksum mismatch; nothing was installed"

    tar -xzf "$dir/$name" -C "$dir"
    sh "$dir/${name%.tar.gz}/install.sh"

    case ":$PATH:" in
        *":$HOME/.local/bin:"*) ;;
        *) echo "Note: ~/.local/bin is not on your PATH, so 'taskdm' needs its full path in a terminal. The app menu entry works either way." ;;
    esac
}

main "$@"
