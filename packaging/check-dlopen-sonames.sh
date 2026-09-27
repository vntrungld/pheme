#!/bin/sh
# Assert that every library pheme opens with `dlopen` is installed and can
# be found by the dynamic loader.
#
# This exists because nothing else can check it. `$auto` in cargo-deb and
# automatic requirement detection in cargo-generate-rpm both read the ELF
# header, so neither sees a library that is never linked -- and the install
# step's `pheme --version` exits inside clap before any of them is opened,
# so it cannot see one either. A package can therefore install cleanly, run
# `--version` cleanly, and still be unable to open a window, which is what
# both `pheme.desktop` and `pheme.service` launch.
#
# Run this inside the container the package was just installed in, with
# nothing else installed. On the development machine, or on a CI runner
# that has just installed the build dependencies, everything resolves and
# the check proves nothing.
#
# The list is derived from the built binary, not remembered:
#
#     readelf -d target/release/pheme | grep NEEDED
#     strings -a target/release/pheme \
#         | grep -oE 'lib[A-Za-z0-9_+.-]*?\.so(\.[0-9]+)*' | sort -u
#
# Every versioned soname the second command prints that the first does not
# is opened with dlopen and belongs below. Keep it in step with
# `[package.metadata.deb] depends` and
# `[package.metadata.generate-rpm.requires]` in crates/pheme-app/Cargo.toml,
# which are the two places the same list is declared to a package manager.
set -eu

sonames="
libayatana-appindicator3.so.1
libEGL.so.1
libGL.so.1
libX11.so.6
libX11-xcb.so.1
libXcursor.so.1
libXi.so.6
libXrender.so.1
libxkbcommon.so.0
libxkbcommon-x11.so.0
libwayland-client.so.0
libwayland-egl.so.1
"

# Report every missing one rather than stopping at the first, so a person
# reading a failed job learns the whole gap in one run.
missing=""
for so in $sonames; do
    if ldconfig -p | grep -qF "$so"; then
        echo "found:   $so"
    else
        echo "MISSING: $so"
        missing="$missing $so"
    fi
done

if [ -n "$missing" ]; then
    echo "undeclared dlopen dependencies:$missing" >&2
    exit 1
fi

echo "all dlopen'd sonames resolve"
