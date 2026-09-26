#!/bin/sh
# Build the .deb inside Debian and install it there, because this is the
# only way to see what `$auto` resolves to: cargo-deb asks dpkg which
# package owns each soname, and there is no dpkg on the development
# machine.
#
# Usage: packaging/verify-deb.sh
# Needs: docker, and a user in the docker group.
set -eu

cd "$(dirname "$0")/.."

docker run --rm -v "$PWD:/w" -w /w rust:1-bookworm sh -eux -c '
    apt-get update
    apt-get install -y --no-install-recommends \
        libx11-dev libxi-dev libxtst-dev libxfixes-dev libxrandr-dev \
        libpipewire-0.3-dev libspa-0.2-dev clang pkg-config \
        libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
        libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev

    # A separate target directory: the host is Arch and its artifacts are
    # linked against a different glibc.
    export CARGO_TARGET_DIR=/w/target-debian

    cargo install cargo-deb --locked
    cargo build --release --locked
    cargo deb -p pheme-app --no-build --no-strip

    deb=$(ls /w/target-debian/debian/pheme_*_amd64.deb)
    echo "--- Depends ---"
    dpkg-deb -f "$deb" Depends
    echo "--- contents ---"
    dpkg-deb -c "$deb"

    # Install it the way a person would, so apt resolves the dependencies
    # rather than dpkg refusing them.
    apt-get install -y "$deb"
    pheme --version

    # Every path the design promises, present exactly once.
    for p in /usr/bin/pheme \
             /usr/lib/udev/rules.d/80-pheme.rules \
             /usr/lib/modules-load.d/pheme.conf \
             /usr/lib/systemd/user/pheme.service \
             /usr/share/applications/pheme.desktop \
             /usr/share/icons/hicolor/32x32/apps/pheme.png; do
        test -f "$p" || { echo "MISSING: $p" >&2; exit 1; }
    done

    # Installing over itself must not fail: postinst runs again on upgrade.
    apt-get install -y --reinstall "$deb"
    pheme --version

    apt-get remove -y pheme
    test ! -e /usr/bin/pheme || { echo "remove left the binary" >&2; exit 1; }
    echo "OK"
'
