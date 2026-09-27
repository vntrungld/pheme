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
# is opened with dlopen. That rule yields fourteen; thirteen of them are
# below, and the fourteenth is left out deliberately:
#
#   libappindicator3.so.1 is the *alternative* half of the tray dependency
#   (`libayatana-appindicator3-1 | libappindicator3-1`). A machine with the
#   modern ayatana library does not have it and does not need it, so
#   asserting it here would fail the check on every machine the package
#   installs correctly on. apt and dnf enforce the either/or; this script
#   only asserts the name the binary prefers.
#
# libxcb.so.1 is here even though libX11.so.6 lists it in its own
# DT_NEEDED, so it cannot be missing wherever libX11 resolves. It costs one
# line to assert the binary's real list rather than a reasoned-down one.
#
# Two of the commands above are worth repeating exactly: the sonames sit in
# rodata with no separator between them, so an anchored regex (`\b`, `^`)
# matches almost nothing. Both reviews of this list under-counted it that
# way -- one found five, the next twelve. Use `grep -oE` as written.
#
# Keep this list in step with `[package.metadata.deb] depends` and
# `[package.metadata.generate-rpm.requires]` in crates/pheme-app/Cargo.toml,
# which are the two places the same list is declared to a package manager.
set -eu

sonames="
libayatana-appindicator3.so.1
libEGL.so.1
libGL.so.1
libX11.so.6
libX11-xcb.so.1
libxcb.so.1
libXcursor.so.1
libXi.so.6
libXrender.so.1
libxkbcommon.so.0
libxkbcommon-x11.so.0
libwayland-client.so.0
libwayland-egl.so.1
"

# With --pid, report which of the same sonames a running pheme has actually
# mapped, by reading /proc/PID/maps. This answers a different question from
# the default mode and is deliberately not pass/fail: which libraries a
# single run opens depends on the session, because eframe asks glutin for
# GLX-then-EGL and winit takes its keymap from the compositor on Wayland.
# So libGL and libxkbcommon-x11 load only under X11, and libEGL only when
# GLX is unavailable or fails. The manual rows F1b and F1c in
# docs/testing.md run this in both session types; the criterion is that
# every soname below is mapped in at least one of the two, which no single
# run can show.
if [ "${1:-}" = "--pid" ]; then
    pid="${2:?usage: $0 --pid PID}"
    maps="/proc/$pid/maps"
    [ -r "$maps" ] || { echo "cannot read $maps" >&2; exit 1; }
    absent=""
    for so in $sonames; do
        # The mapped file carries the full version ("libGL.so.1.7.0"), so
        # match the soname as a prefix rather than for equality.
        if grep -qE "/${so}([.0-9]*)\$" "$maps" || grep -qF "/$so" "$maps"; then
            echo "mapped:     $so"
        else
            echo "not mapped: $so"
            absent="$absent $so"
        fi
    done
    if [ -n "$absent" ]; then
        echo "not opened by this run:$absent"
        echo "(expected for the other session type -- see the comment above)"
    else
        echo "this run opened all of them"
    fi
    exit 0
fi

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
