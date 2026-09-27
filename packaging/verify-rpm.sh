#!/bin/sh
# Build the .rpm and then install it, in two separate containers.
#
# Unlike the .deb the build could happen on the development machine --
# cargo-generate-rpm needs no rpm tooling, only an ELF reader -- but the
# binary would be linked against Arch's glibc, so the build happens in
# Fedora instead.
#
# The install happens in a second, bare container. The build container has
# every `-devel` package installed, and each of those pulls in the runtime
# library beside it, so every requirement the package could possibly
# declare is already satisfied there -- a wrong or missing `Requires`
# cannot fail an install in the container that just built it. The second
# container has nothing but the package, so dnf has to resolve the
# requirements for real.
#
# Usage: packaging/verify-rpm.sh
# Needs: docker, and a user in the docker group.
set -eu

cd "$(dirname "$0")/.."

echo "=== build (fedora:latest, with the -devel packages) ==="
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

    # Pinned to the version release.yml pins, so this script builds the
    # same artifact, with the same filename, that CI publishes.
    cargo install cargo-generate-rpm@0.21.0 --locked
    cargo build --release --locked
    cargo generate-rpm -p crates/pheme-app

    rpm=$(ls /w/target-fedora/generate-rpm/pheme-*.rpm)
    echo "--- Requires ---"
    rpm -qp --requires "$rpm"
    echo "--- contents ---"
    rpm -qlp "$rpm"
'

echo "=== install (a clean fedora:latest, with nothing) ==="
# The mount is read-only: this container must not be able to leave anything
# behind in the checkout, and it has no reason to.
docker run --rm -v "$PWD:/w:ro" fedora:latest sh -eux -c '
    rpm=$(ls /w/target-fedora/generate-rpm/pheme-*.rpm)

    dnf install -y "$rpm"
    pheme --version

    # --version exits inside clap, before the first dlopen, so it proves
    # nothing about the twelve libraries that are opened that way. This is
    # the check that can fail.
    sh /w/packaging/check-dlopen-sonames.sh

    # Again, over the top. dnf re-runs post_install_script on an upgrade and
    # the .deb'\''s script proves the same property for postinst; without this
    # the two packages are not verified to the same standard.
    dnf reinstall -y "$rpm"
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
