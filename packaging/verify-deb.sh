#!/bin/sh
# Build the .deb and then install it, in two separate containers.
#
# The build has to happen inside Ubuntu, because this is the only way to
# see what `$auto` resolves to: cargo-deb asks dpkg which package owns each
# soname, and there is no dpkg on the development machine. Ubuntu 24.04
# specifically, and not a Debian image: it is what CI's own runner is, so
# this validates the same package CI ships (with Ubuntu 24.04's post-time_t
# "t64" library names) rather than a Debian-named package nobody will
# actually download.
#
# The install has to happen somewhere else. The build container has every
# `-dev` package installed, and each of those pulls in the runtime library
# beside it, so every dependency the package could possibly declare is
# already satisfied there -- a wrong or missing `Depends` cannot fail an
# install in the container that just built it. The second container starts
# from a bare ubuntu:24.04 and has nothing but the package, so apt has to
# resolve the dependency list for real.
#
# Usage: packaging/verify-deb.sh
# Needs: docker, and a user in the docker group.
set -eu

cd "$(dirname "$0")/.."

echo "=== build (ubuntu:24.04, with the -dev packages) ==="
docker run --rm -v "$PWD:/w" -w /w ubuntu:24.04 sh -eux -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update
    # Ubuntu 24.04 ships no Rust new enough to build this workspace, so
    # install one with rustup instead of the distro package, the same way
    # a from-source Linux build (see the README) would.
    apt-get install -y --no-install-recommends \
        curl ca-certificates build-essential \
        libx11-dev libxi-dev libxtst-dev libxfixes-dev libxrandr-dev \
        libpipewire-0.3-dev libspa-0.2-dev clang pkg-config \
        libgtk-3-dev libayatana-appindicator3-dev libxdo-dev \
        libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxkbcommon-dev

    curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --profile minimal
    . "$HOME/.cargo/env"

    # A separate target directory: the host is Arch and its artifacts are
    # linked against a different glibc.
    export CARGO_TARGET_DIR=/w/target-debian

    # The container runs as root, so anything it creates under the bind
    # mount is root-owned on the host. Hand it back on the way out, whether
    # or not the rest of the script succeeds.
    trap '\''chown -R "$(stat -c %u:%g /w)" /w/target-debian 2>/dev/null || true'\'' EXIT

    cargo install cargo-deb --locked
    cargo build --release --locked
    cargo deb -p pheme-app --no-build --no-strip

    deb=$(ls /w/target-debian/debian/pheme_*_amd64.deb)
    echo "--- Depends ---"
    dpkg-deb -f "$deb" Depends
    echo "--- contents ---"
    dpkg-deb -c "$deb"
'

echo "=== install (a clean ubuntu:24.04, with nothing) ==="
# The mount is read-only: this container must not be able to leave anything
# behind in the checkout, and it has no reason to.
docker run --rm -v "$PWD:/w:ro" ubuntu:24.04 sh -eux -c '
    export DEBIAN_FRONTEND=noninteractive
    apt-get update

    deb=$(ls /w/target-debian/debian/pheme_*_amd64.deb)

    # Install it the way a person would, so apt resolves the dependencies
    # rather than dpkg refusing them.
    apt-get install -y "$deb"
    pheme --version

    # --version exits inside clap, before the first dlopen, so it proves
    # nothing about the twelve libraries that are opened that way. This is
    # the check that can fail.
    sh /w/packaging/check-dlopen-sonames.sh

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
