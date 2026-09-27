#!/bin/sh
# Build the .rpm and install it in Fedora. Unlike the .deb this could be
# built on the development machine -- cargo-generate-rpm needs no rpm
# tooling, only an ELF reader -- but the binary would be linked against
# Arch's glibc, so the build happens in the container that will install it.
#
# Usage: packaging/verify-rpm.sh
# Needs: docker, and a user in the docker group.
set -eu

cd "$(dirname "$0")/.."

docker run --rm -v "$PWD:/w" -w /w fedora:latest sh -eux -c '
    dnf install -y --setopt=install_weak_deps=False \
        cargo rust clang pkgconf-pkg-config \
        libX11-devel libXi-devel libXtst-devel libXfixes-devel libXrandr-devel \
        pipewire-devel gtk3-devel libayatana-appindicator-gtk3-devel \
        libxdo-devel libxkbcommon-devel

    export CARGO_TARGET_DIR=/w/target-fedora
    # The container runs as root, so anything it creates under the bind
    # mount is root-owned on the host. Hand it back on the way out, whether
    # or not the rest of the script succeeds.
    trap '\''chown -R "$(stat -c %u:%g /w)" /w/target-fedora 2>/dev/null || true'\'' EXIT

    cargo install cargo-generate-rpm --locked
    cargo build --release --locked
    cargo generate-rpm -p crates/pheme-app

    rpm=$(ls /w/target-fedora/generate-rpm/pheme-*.rpm)
    echo "--- Requires ---"
    rpm -qp --requires "$rpm"
    echo "--- contents ---"
    rpm -qlp "$rpm"

    dnf install -y "$rpm"
    pheme --version

    for p in /usr/bin/pheme \
             /usr/lib/udev/rules.d/80-pheme.rules \
             /usr/lib/modules-load.d/pheme.conf \
             /usr/lib/systemd/user/pheme.service \
             /usr/share/applications/pheme.desktop \
             /usr/share/icons/hicolor/32x32/apps/pheme.png; do
        test -f "$p" || { echo "MISSING: $p" >&2; exit 1; }
    done

    dnf remove -y pheme
    test ! -e /usr/bin/pheme || { echo "remove left the binary" >&2; exit 1; }
    echo "OK"
'
