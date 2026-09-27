# Packaging Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** pheme installs with one command on Debian, Ubuntu and Fedora and with one double-click on Windows, without the person first reading a list of libraries to install.

**Architecture:** `cargo-deb` and `cargo-generate-rpm` build native packages from metadata in `crates/pheme-app/Cargo.toml`, with the data files they install living in `packaging/`. Both derive their runtime dependencies from the binary that was actually built — `cargo-deb` by asking `dpkg` which package owns each soname `ldd` reports, `cargo-generate-rpm` by reading the ELF header directly — so the dependency list is derived rather than remembered. Windows gets an Inno Setup installer that needs no administrator. CI then installs each package and runs the binary out of it, which is this sub-project's only real test.

**Tech Stack:** `cargo-deb` 3.8, `cargo-generate-rpm` 0.21, Inno Setup 6, GitHub Actions, Docker (for local verification against Debian and Fedora).

**Spec:** `docs/superpowers/specs/2026-09-26-packaging-design.md`

## Global Constraints

- All documents, code, comments and commit messages in this repository are in **English**.
- Commit format is `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`; title under 72 characters, imperative mood; blank line; body wrapped at 72 columns; trailer exactly `Co-Authored-By: Claude <noreply@anthropic.com>` and nothing else.
- `Cargo.lock` is committed with **any** manifest change. `release.yml` builds `--locked`.
- CI runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on **both** `ubuntu-latest` and `windows-latest`. Anything added under `crates/pheme-app/tests/` must compile and pass on Windows too.
- The packaged udev rule and modules file must be byte-identical to `pheme_app::setup::UDEV_RULE` and `pheme_app::setup::MODULES_LOAD`, pinned by a test.
- `pheme setup` must not gain or lose behaviour in this sub-project. It stays for people who install from the tarball.
- The Windows installer must not require administrator: `PrivilegesRequired=lowest`.
- Nothing is code-signed. The README says so plainly rather than suggesting a workaround.
- The existing tarball and zip artifacts stay. Packages are added beside them, not instead of them.

**This machine cannot build or install either package.** It is Arch (CachyOS): no `dpkg`, no `rpm`, no `iscc`. `cargo deb`'s default `$auto` shells out to `dpkg-query` and will fail here. Docker **is** available and the user is in the `docker` group, so the Linux packages are verified by building inside a container — that is the only honest local verification, and Tasks 2 and 3 each do it for their own package. The Inno Setup script cannot be compiled locally at all; CI is its first compile, and Task 4 must say so rather than imply otherwise.

**Two path-resolution rules that differ between the two tools.** Getting either wrong produces a package that builds and is missing files.

- `cargo-deb` assets are `["source", "dest", "mode"]`. `source` resolves against the **package manifest directory** (`crates/pheme-app/`), so a file at the repository root needs `../../`. The single exception is the binary: write `target/release/pheme` literally, with no `../../`, because cargo-deb matches that exact prefix to decide what to build and substitutes the real target directory itself. Its own README warns that "fixing" the path breaks it. `dest` is relative, with no leading slash; a `dest` ending in `/` keeps the source's file name.
- `cargo-generate-rpm` assets are `{ source = "...", dest = "...", mode = "..." }`. `source` resolves against the **current working directory**, which is the workspace root when the command is run from there, so no `../../`. `dest` is absolute.

## Review Focus

1. **A dependency that `ldd` cannot see.** `libayatana-appindicator` is `dlopen`'d by the tray at runtime, not linked — confirmed by `ldd`, which lists GTK and GDK but no appindicator. Automatic detection reads the ELF header and will never find it. Expected: it is declared by hand in both packages, and installing the package pulls it in. — Tasks 2 and 3.
2. **The packaged udev rule drifting from the constant `pheme setup` writes.** Two copies of the same text, in different files, with nothing forcing them equal. Expected: a test fails the moment they differ. — Task 1.
3. **Installing over an existing install.** `postinst` runs again on upgrade, and the Windows PATH task runs again on a repeat install. Expected: the modprobe and udev steps are idempotent, and PATH gains the directory once rather than twice. — Tasks 2 and 4.
4. **Removing the package.** Expected: `apt purge` takes the binary, the unit, the udev rule and the desktop entry, and leaves `~/.config/pheme/config.toml`; the Windows uninstaller takes the Run value and the PATH entry and leaves `%APPDATA%\pheme\config.toml`. — Tasks 2 and 4.
5. **A tag that disagrees with the crate version.** Tagging `v0.2.0` against a crate at `0.1.0` produces `pheme_0.1.0_amd64.deb` inside a release called `v0.2.0`. Expected: the release job fails before publishing anything. — Task 5.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `packaging/linux/80-pheme.rules` | udev rule, byte-identical to `setup::UDEV_RULE` |
| `packaging/linux/pheme-modules.conf` | modules-load entry, byte-identical to `setup::MODULES_LOAD` |
| `packaging/linux/pheme.service` | systemd `--user` unit, shipped disabled |
| `packaging/linux/pheme.desktop` | application menu entry |
| `packaging/debian/postinst` | udev reload and module load after install |
| `packaging/windows/pheme.iss` | Inno Setup script |
| `packaging/verify-deb.sh` | build and install the `.deb` in a Debian container |
| `packaging/verify-rpm.sh` | build and install the `.rpm` in a Fedora container |
| `crates/pheme-app/tests/packaging.rs` | pins the data files to the constants they mirror |

**Modified:**

| File | Change |
|---|---|
| `Cargo.toml` | correct `repository` |
| `crates/pheme-app/Cargo.toml` | `description`, `[package.metadata.deb]`, `[package.metadata.generate-rpm]` |
| `.github/workflows/release.yml` | build, verify and publish the packages and the installer |
| `README.md` | install from a package, the user unit, the SmartScreen warning |
| `docs/testing.md` | rows F1–F12 |

---

### Task 1: The data files, and the metadata both formats read

**Files:**
- Create: `packaging/linux/80-pheme.rules`
- Create: `packaging/linux/pheme-modules.conf`
- Create: `packaging/linux/pheme.service`
- Create: `packaging/linux/pheme.desktop`
- Create: `crates/pheme-app/tests/packaging.rs`
- Modify: `Cargo.toml`
- Modify: `crates/pheme-app/Cargo.toml`

**Interfaces:**
- Consumes: `pheme_app::setup::UDEV_RULE` and `pheme_app::setup::MODULES_LOAD`, both `pub` and both unconditional (they are not behind a `cfg`, so this test compiles on Windows).
- Produces: the four files under `packaging/linux/`, which Tasks 2 and 3 install; `package.description`, which both package formats require.

- [ ] **Step 1: Write the failing test**

Create `crates/pheme-app/tests/packaging.rs`:

```rust
//! The packaged data files against the constants they mirror.
//!
//! `pheme setup` writes the udev rule and the modules entry at runtime; the
//! `.deb` and the `.rpm` ship the same text as files. Two copies of one
//! string in two places is a drift waiting to happen, and the drift is
//! silent: a package would install a rule that no longer matches what the
//! program expects, and nothing would say so until somebody's /dev/uinput
//! was unreadable.

use std::path::{Path, PathBuf};

/// The repository root, from this crate's manifest directory.
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .canonicalize()
        .expect("the workspace root is two levels above crates/pheme-app")
}

fn read(rel: &str) -> String {
    let p = repo_root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading {}: {e}", p.display()))
}

/// Break it by changing one character in either file: the packaged rule and
/// the rule `pheme setup` writes would then differ, and a machine set up by
/// the package would behave differently from one set up by the command.
#[test]
fn the_packaged_udev_rule_is_the_one_setup_writes() {
    assert_eq!(read("packaging/linux/80-pheme.rules"), pheme_app::setup::UDEV_RULE);
}

/// Break it the same way. `MODULES_LOAD` names both uinput and i2c-dev; a
/// package shipping only one leaves a machine without either virtual input
/// devices or DDC/CI, depending which went missing.
#[test]
fn the_packaged_modules_entry_is_the_one_setup_writes() {
    assert_eq!(
        read("packaging/linux/pheme-modules.conf"),
        pheme_app::setup::MODULES_LOAD
    );
}

/// Break it by changing `Exec=` to anything but the installed binary's
/// name: the menu entry would then launch nothing, which a person discovers
/// by clicking an icon that does not start the program.
#[test]
fn the_desktop_entry_launches_the_binary_the_package_installs() {
    let desktop = read("packaging/linux/pheme.desktop");
    assert!(
        desktop.lines().any(|l| l == "Exec=pheme"),
        "no `Exec=pheme` line in:\n{desktop}"
    );
    // The three keys the desktop entry specification requires of every
    // Type=Application entry. A file missing any of them is skipped
    // silently by the menu rather than reported.
    for key in ["Type=Application", "Name=Pheme", "Icon=pheme"] {
        assert!(desktop.lines().any(|l| l == key), "no `{key}` line in:\n{desktop}");
    }
}

/// Break it by pointing `ExecStart` anywhere but where the packages put the
/// binary: `systemctl --user start pheme` then fails with a message about a
/// missing executable, on a machine where the program is installed.
#[test]
fn the_unit_starts_the_binary_where_the_packages_install_it() {
    let unit = read("packaging/linux/pheme.service");
    assert!(
        unit.lines().any(|l| l == "ExecStart=/usr/bin/pheme"),
        "no `ExecStart=/usr/bin/pheme` line in:\n{unit}"
    );
}

/// The unit is shipped so a person can enable it, not enabled by the
/// package. Break it by changing either line to name `default.target`, or
/// by adding a second `WantedBy=` beside the right one: `systemctl --user
/// enable` would then hook the unit into a target that exists with no
/// graphical session, and start a GUI front-end on a machine with nothing
/// to draw into.
///
/// The exclusivity assertion is what makes the second half of that claim
/// true. Presence assertions alone pass for a unit naming both targets,
/// and a unit naming both is exactly the broken case -- systemd honours
/// every `WantedBy` it finds.
#[test]
fn the_unit_belongs_to_the_graphical_session() {
    let unit = read("packaging/linux/pheme.service");
    assert!(unit.lines().any(|l| l == "WantedBy=graphical-session.target"));
    assert!(unit.lines().any(|l| l == "PartOf=graphical-session.target"));
    let wanted: Vec<&str> = unit
        .lines()
        .filter(|l| l.starts_with("WantedBy="))
        .collect();
    assert_eq!(
        wanted,
        ["WantedBy=graphical-session.target"],
        "the unit must name exactly one WantedBy"
    );
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pheme-app --test packaging`
Expected: FAIL — `reading .../packaging/linux/80-pheme.rules: No such file or directory`.

- [ ] **Step 3: Write the four data files**

The first two are generated from the constants rather than typed, so they cannot be typed wrong. Run this from the repository root:

```bash
mkdir -p packaging/linux packaging/debian packaging/windows
cargo run -q -p pheme-app --bin pheme -- --version >/dev/null   # ensure it builds
python3 - <<'PY'
import re, pathlib
src = pathlib.Path("crates/pheme-app/src/setup.rs").read_text()
def const(name):
    m = re.search(rf'pub const {name}: &str = "((?:[^"\\]|\\.)*)";', src)
    return m.group(1).encode().decode("unicode_escape")
pathlib.Path("packaging/linux/80-pheme.rules").write_text(const("UDEV_RULE"))
pathlib.Path("packaging/linux/pheme-modules.conf").write_text(const("MODULES_LOAD"))
PY
```

`packaging/linux/pheme.service`:

```ini
[Unit]
Description=Pheme keyboard, mouse and audio sharing
Documentation=https://github.com/vntrungld/pheme
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/pheme
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
```

`Restart=on-failure` rather than `always`: the front-end exiting because somebody chose Quit from the tray is not a failure to be undone.

`packaging/linux/pheme.desktop`:

```ini
[Desktop Entry]
Type=Application
Name=Pheme
Comment=Share one keyboard, mouse, clipboard and audio between two machines
Exec=pheme
Icon=pheme
Terminal=false
Categories=Utility;Network;RemoteAccess;
Keywords=kvm;keyboard;mouse;share;
```

- [ ] **Step 4: Fix the two metadata defects**

In the root `Cargo.toml`, under `[workspace.package]`, the repository URL names an account that does not own this repository — the remote is `git@github.com:vntrungld/pheme.git`. Both package formats embed it:

```toml
repository = "https://github.com/vntrungld/pheme"
```

In `crates/pheme-app/Cargo.toml`, under `[package]`, add the description both formats require:

```toml
description = "Share one keyboard, mouse, clipboard and audio between two machines over the LAN"
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --test packaging`
Expected: PASS, 5 tests.

- [ ] **Step 6: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 7: Commit**

```bash
git add packaging Cargo.toml Cargo.lock crates/pheme-app/Cargo.toml \
        crates/pheme-app/tests/packaging.rs
git commit -F - <<'EOF'
Update: add the files the Linux packages will install

The udev rule and the modules-load entry are generated from
setup::UDEV_RULE and setup::MODULES_LOAD rather than typed, and a test
asserts they stay byte-identical. Two copies of one string in two files
drift silently: a package would install a rule that no longer matches
what the program expects, and nothing would say so until somebody's
/dev/uinput was unreadable.

The systemd unit is a user unit and is shipped disabled. A user unit
belongs to a person rather than to a machine, and enabling it is
`systemctl --user enable --now pheme`. It is PartOf and WantedBy
graphical-session.target because it starts the front-end, which draws a
window; Restart=on-failure rather than always, because quitting from the
tray is not a failure to undo.

Also corrects two things both package formats read: the repository URL
named an account that does not own this repository, and pheme-app had no
description, which .deb and .rpm both require.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 2: The `.deb`

**Files:**
- Modify: `crates/pheme-app/Cargo.toml`
- Create: `packaging/debian/postinst`
- Create: `packaging/verify-deb.sh`

**Interfaces:**
- Consumes: the four files from Task 1.
- Produces: `cargo deb -p pheme-app` builds `target/debian/pheme_<version>_amd64.deb`; `packaging/verify-deb.sh` builds and installs it in a container, which Task 5 mirrors in CI.

Read the two path-resolution rules in the Global Constraints before writing the asset list. They are the most likely thing to get wrong here.

- [ ] **Step 1: Write the cargo-deb metadata**

Append to `crates/pheme-app/Cargo.toml`:

```toml
# The binary's own path is written `target/release/pheme` on purpose, with
# no `../../`: cargo-deb matches that exact prefix to work out what to
# build and substitutes the real target directory itself, and its own
# README warns that "correcting" the path breaks it. Every other source is
# relative to this manifest's directory, which is why they carry `../../`.
[package.metadata.deb]
name = "pheme"
maintainer = "Lam Duc Trung <oedevai7@gmail.com>"
copyright = "2026 Lam Duc Trung"
license-file = ["../../LICENSE", "0"]
section = "utils"
priority = "optional"
extended-description = """
Pheme shares one keyboard, mouse, clipboard and audio stream between two \
machines on the same network. The pointer crosses the screen edge and the \
input follows it; a monitor cabled to both machines can follow it too."""
# $auto asks dpkg which package provides each soname ldd reports, so the
# dependency list is derived from the binary that was built rather than
# remembered. It cannot see libayatana-appindicator, which the tray opens
# with dlopen at runtime rather than linking, so that one is named here.
depends = "$auto, libayatana-appindicator3-1 | libappindicator3-1"
assets = [
    ["target/release/pheme", "usr/bin/", "755"],
    ["../../packaging/linux/80-pheme.rules", "usr/lib/udev/rules.d/", "644"],
    ["../../packaging/linux/pheme-modules.conf", "usr/lib/modules-load.d/pheme.conf", "644"],
    ["../../packaging/linux/pheme.service", "usr/lib/systemd/user/", "644"],
    ["../../packaging/linux/pheme.desktop", "usr/share/applications/", "644"],
    ["assets/tray-connected.png", "usr/share/icons/hicolor/32x32/apps/pheme.png", "644"],
    ["../../README.md", "usr/share/doc/pheme/", "644"],
    ["../../docs/testing.md", "usr/share/doc/pheme/", "644"],
]
maintainer-scripts = "../../packaging/debian/"
```

`license-file` is why `LICENSE` is not in the asset list: cargo-deb turns
it into `/usr/share/doc/pheme/copyright`, which is where Debian policy
puts it and where a Debian user looks. The `.rpm` in Task 3 ships it as
`/usr/share/doc/pheme/LICENSE` instead, which is the RPM convention. The
two packages differ here on purpose.

There is deliberately no `systemd-units` key. cargo-deb's systemd support is for **system** units — `SystemdUnitsConfig` has no notion of a user unit, which was checked in its source rather than assumed — and a user unit should not be enabled by a package anyway. The unit goes in as a plain asset and stays disabled.

- [ ] **Step 2: Write the postinst**

`packaging/debian/postinst`, mode `755`:

```sh
#!/bin/sh
set -e

# The same sequence, in the same order, that the root path of `pheme setup`
# runs, and for the same reason: modprobe creates the device nodes, the
# reload picks up the rule file just installed, and the triggers reprocess
# those nodes against it. Without the triggers the rule applies to nothing
# until a reboot.
#
# Every step tolerates failure. A container has no /sys to trigger against
# and no module to load, and a package install must not fail there.
if [ "$1" = "configure" ]; then
    modprobe uinput || true
    modprobe i2c-dev || true
    udevadm control --reload || true
    udevadm trigger --name-match=uinput || true
    udevadm trigger --subsystem-match=i2c-dev || true
fi

#DEBHELPER#
```

It adds nobody to a group. `pheme setup` adds the invoking user to `input` because the program cannot work at all without it, but a package installs system-wide with no invoking user, and `TAG+="uaccess"` covers the logged-in user on any system with systemd-logind.

- [ ] **Step 3: Write the verification script**

`packaging/verify-deb.sh`, mode `755`. This machine has no `dpkg`, so the build happens inside a Debian container — that is the only way to see what `$auto` actually resolves to.

```sh
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
```

- [ ] **Step 4: Run it**

Run: `packaging/verify-deb.sh`
Expected: it ends with `OK`. The first run downloads the image and compiles the workspace inside the container, so allow several minutes.

Read the `--- Depends ---` line it prints and put it in your report. That line is the whole point of this sub-project: it should name `libgtk-3-0`, `libpipewire-0.3-0` and the appindicator package, none of which anybody typed into a list.

If the install fails on a missing library, that is a real finding — the `depends` line is incomplete. Report it rather than adding the library to the tarball's README.

- [ ] **Step 5: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 6: Commit**

```bash
git add crates/pheme-app/Cargo.toml Cargo.lock packaging/debian packaging/verify-deb.sh
git commit -F - <<'EOF'
Update: build a .deb that declares its own dependencies

cargo-deb's default for Depends is $auto: it runs ldd over the binary
that was actually built and asks dpkg which package provides each
soname. The dependency list is therefore derived rather than remembered,
which is the point -- the README's hand-written list of libraries to
install first is wrong the moment a dependency changes and nobody
notices until somebody's binary dies at load time.

libayatana-appindicator is named by hand because $auto cannot see it:
the tray opens it with dlopen at runtime rather than linking it, so it
is absent from the ELF header automatic detection reads.

There is no systemd-units key. cargo-deb's systemd support is for system
units and has no notion of a user unit, and a user unit should not be
enabled by a package in any case; it ships as a plain asset, disabled.

postinst repeats the sequence the root path of `pheme setup` runs, in
the same order and tolerant of failure, so that installing the package
leaves /dev/uinput and /dev/i2c-* usable without a reboot.

verify-deb.sh builds and installs the package inside a Debian container,
which is the only way to see what $auto resolves to: there is no dpkg on
the development machine.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 3: The `.rpm`

**Files:**
- Modify: `crates/pheme-app/Cargo.toml`
- Create: `packaging/verify-rpm.sh`

**Interfaces:**
- Consumes: the four files from Task 1.
- Produces: `cargo generate-rpm -p crates/pheme-app` builds `target/generate-rpm/pheme-<version>-1.x86_64.rpm`; `packaging/verify-rpm.sh`, which Task 5 mirrors in CI.

`cargo-generate-rpm` resolves asset sources against the **current working directory**, not the manifest directory — the opposite of cargo-deb — so these paths carry no `../../`. Its assets are inline tables, and `dest` is absolute. Both differences are easy to miss when copying the `.deb` block.

- [ ] **Step 1: Write the cargo-generate-rpm metadata**

Append to `crates/pheme-app/Cargo.toml`:

```toml
# Asset sources here resolve against the working directory the command is
# run from -- the workspace root -- which is why these carry no `../../`,
# unlike the cargo-deb block above. `dest` is absolute here and relative
# there. The two tools disagree about both.
[package.metadata.generate-rpm]
name = "pheme"
summary = "Share one keyboard, mouse, clipboard and audio between two machines"
assets = [
    { source = "target/release/pheme", dest = "/usr/bin/pheme", mode = "755" },
    { source = "packaging/linux/80-pheme.rules", dest = "/usr/lib/udev/rules.d/80-pheme.rules", mode = "644" },
    { source = "packaging/linux/pheme-modules.conf", dest = "/usr/lib/modules-load.d/pheme.conf", mode = "644" },
    { source = "packaging/linux/pheme.service", dest = "/usr/lib/systemd/user/pheme.service", mode = "644" },
    { source = "packaging/linux/pheme.desktop", dest = "/usr/share/applications/pheme.desktop", mode = "644" },
    { source = "crates/pheme-app/assets/tray-connected.png", dest = "/usr/share/icons/hicolor/32x32/apps/pheme.png", mode = "644" },
    { source = "README.md", dest = "/usr/share/doc/pheme/README.md", mode = "644" },
    { source = "docs/testing.md", dest = "/usr/share/doc/pheme/testing.md", mode = "644" },
    { source = "LICENSE", dest = "/usr/share/doc/pheme/LICENSE", mode = "644" },
]
post_install_script = """
modprobe uinput || true
modprobe i2c-dev || true
udevadm control --reload || true
udevadm trigger --name-match=uinput || true
udevadm trigger --subsystem-match=i2c-dev || true
"""

# The soname rather than a package name: what provides it differs between
# Fedora and openSUSE, the soname does not. Automatic requirement detection
# cannot find this one, because the tray opens it with dlopen rather than
# linking it.
[package.metadata.generate-rpm.requires]
"libayatana-appindicator3.so.1()(64bit)" = "*"
```

`auto-req` is left at its default. On a host with no `/usr/lib/rpm/find-requires` — which is every Ubuntu runner and this development machine — `AutoReqMode::Auto` falls back to the crate's built-in ELF reader, which is what makes an `.rpm` buildable from a Debian-family host at all.

- [ ] **Step 2: Write the verification script**

`packaging/verify-rpm.sh`, mode `755`:

```sh
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
    trap 'chown -R "$(stat -c %u:%g /w)" /w/target-fedora 2>/dev/null || true' EXIT

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

    # Again, over the top. dnf re-runs post_install_script on an upgrade and
    # the .deb's script proves the same property for postinst; without this
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
```

- [ ] **Step 3: Run it**

Run: `packaging/verify-rpm.sh`
Expected: it ends with `OK`.

Put the `--- Requires ---` list in your report. It should be soname requirements — `libgtk-3.so.0()(64bit)` and the rest — produced by the built-in reader, plus the appindicator line declared by hand.

`cargo generate-rpm` does not build; the script builds first. If the `-p` argument is rejected, check whether it takes a path or a package name in the version installed, and report what you found rather than guessing.

- [ ] **Step 4: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 5: Commit**

```bash
git add crates/pheme-app/Cargo.toml Cargo.lock packaging/verify-rpm.sh
git commit -F - <<'EOF'
Update: build an .rpm that declares its own dependencies

cargo-generate-rpm's automatic requirement detection falls back to its
built-in ELF reader when it finds no /usr/lib/rpm/find-requires, which
is the case on every Debian-family host. That fallback is what lets one
Ubuntu runner produce both packages.

Its asset paths resolve against the working directory rather than the
manifest directory, and its dest is absolute rather than relative -- the
opposite of cargo-deb on both counts, which the comment in the manifest
records so the two blocks are not copied into each other.

The appindicator requirement is written as a soname rather than a
package name, because what provides it differs between Fedora and
openSUSE while the soname does not. Automatic detection cannot find it
either way: the tray opens it with dlopen rather than linking it.

verify-rpm.sh builds and installs inside Fedora. The package could be
built on the development machine, since no rpm tooling is needed, but
the binary would be linked against Arch's glibc.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 4: The Windows installer

**Files:**
- Create: `packaging/windows/pheme.iss`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `iscc /DAppVersion=<v> packaging/windows/pheme.iss` writes `packaging/windows/Output/pheme-<v>-x86_64-windows-setup.exe`, which Task 5 builds and exercises in CI.

**You cannot compile this here.** There is no `iscc` on this machine and no Windows. CI is its first compile. Write it carefully, check it by reading, and say plainly in your report that it is unverified rather than implying otherwise — an untested installer described as working is worse than one described as untested.

- [ ] **Step 1: Write the script**

`packaging/windows/pheme.iss`:

```pascal
; Built by CI with:  iscc /DAppVersion=0.1.0 packaging\windows\pheme.iss
; AppVersion is required; the script refuses to compile without it rather
; than bake in a version that will quietly go stale.
#ifndef AppVersion
  #error AppVersion must be passed with /DAppVersion=x.y.z
#endif

[Setup]
AppId={{9C8F3B21-6E4A-4E5D-9C3A-7F2D5A1B8E40}
AppName=Pheme
AppVersion={#AppVersion}
AppPublisher=Lam Duc Trung
AppPublisherURL=https://github.com/vntrungld/pheme
DefaultDirName={autopf}\Pheme
DefaultGroupName=Pheme
DisableProgramGroupPage=yes
; No administrator. With PrivilegesRequired=lowest, {autopf} resolves to
; %LOCALAPPDATA%\Programs and no UAC prompt appears. Pheme creates no
; service and writes nothing outside the user's own profile, and an
; installer that asks for administrator teaches people to grant it.
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
; Tells Windows to broadcast the environment change, so a terminal opened
; after the install sees the new PATH without a sign-out.
ChangesEnvironment=yes
OutputDir=Output
OutputBaseFilename=pheme-{#AppVersion}-x86_64-windows-setup
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
LicenseFile=..\..\LICENSE

[Files]
Source: "..\..\target\release\pheme.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\README.md"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\LICENSE"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\docs\testing.md"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Pheme"; Filename: "{app}\pheme.exe"
Name: "{group}\Uninstall Pheme"; Filename: "{uninstallexe}"

[Tasks]
Name: "startup"; Description: "Start Pheme when I sign in"
Name: "addtopath"; Description: "Add Pheme to PATH (for pheme displays, pheme pair)"; Flags: unchecked

[Registry]
; The key Task Manager's Startup tab reads, so somebody who changes their
; mind finds the switch where they will look for it.
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; \
    ValueType: string; ValueName: "Pheme"; ValueData: """{app}\pheme.exe"""; \
    Flags: uninsdeletevalue; Tasks: startup
; Guarded by NeedsAddPath so a repeat install appends once, not twice.
; Two entries with complementary checks rather than one: on a profile that
; has never had a user Path, {olddata} expands to nothing and a single
; entry would write ";C:\...", whose empty leading segment Windows resolves
; as the current directory.
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{olddata};{app}"; Tasks: addtopath; \
    Check: NeedsAddPath(ExpandConstant('{app}')) and HasExistingPath
Root: HKCU; Subkey: "Environment"; ValueType: expandsz; ValueName: "Path"; \
    ValueData: "{app}"; Tasks: addtopath; \
    Check: NeedsAddPath(ExpandConstant('{app}')) and not HasExistingPath

[Code]
const
  EnvironmentKey = 'Environment';

{ True when {app} is not already one of the user's PATH entries. The
  comparison pads both sides with ';' so the first and last entries match
  the same way every middle one does. }
function NeedsAddPath(Param: string): Boolean;
var
  OrigPath: string;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath) then
  begin
    Result := True;
    Exit;
  end;
  Result := Pos(';' + Uppercase(Param) + ';', ';' + Uppercase(OrigPath) + ';') = 0;
end;

{ True when the user already has a non-empty Path, so a new entry needs a
  separator in front of it. Without this test, {olddata} expands to nothing
  on a profile that never had one and the value becomes ";C:\...", whose
  empty leading segment Windows resolves as the current directory. }
function HasExistingPath(): Boolean;
var
  OrigPath: string;
begin
  Result := RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', OrigPath)
            and (OrigPath <> '');
end;

{ Take {app} back out of PATH without disturbing anything else in it. }
procedure RemovePath(Path: string);
var
  Paths: string;
  P: Integer;
begin
  if not RegQueryStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths) then
    Exit;
  Paths := ';' + Paths + ';';
  P := Pos(';' + Uppercase(Path) + ';', Uppercase(Paths));
  if P = 0 then
    Exit;
  { Delete the entry and the ';' that followed it, then the two sentinels. }
  Delete(Paths, P, Length(Path) + 1);
  Delete(Paths, 1, 1);
  if (Length(Paths) > 0) and (Paths[Length(Paths)] = ';') then
    Delete(Paths, Length(Paths), 1);
  RegWriteExpandStringValue(HKEY_CURRENT_USER, EnvironmentKey, 'Path', Paths);
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  if CurUninstallStep = usPostUninstall then
    RemovePath(ExpandConstant('{app}'));
end;
```

Nothing here deletes `%APPDATA%\pheme\config.toml`. An uninstaller that throws away a person's configuration is one they learn to fear, and the Run value and the PATH entry are the only things outside `{app}` this installer creates.

- [ ] **Step 2: Check the PATH arithmetic by hand**

Neither this nor Step 1 can be run here, so trace `RemovePath` on paper and put the trace in your report. Three cases, all of which a careless version gets wrong:

- `Paths` is `C:\A;C:\B;C:\App`, removing `C:\App` — the entry is last.
- `Paths` is `C:\App`, removing `C:\App` — it is the only entry.
- `Paths` is `C:\A;C:\B`, removing `C:\App` — it is not there at all.

State what the registry value ends up as in each. If any case leaves a stray `;` or eats a neighbouring entry, fix the code before committing.

- [ ] **Step 3: Commit**

```bash
git add packaging/windows/pheme.iss
git commit -F - <<'EOF'
Update: add a Windows installer that needs no administrator

PrivilegesRequired=lowest puts pheme in %LOCALAPPDATA%\Programs with no
UAC prompt. Pheme creates no service and writes nothing outside the
user's own profile, and an installer that asks for administrator teaches
people to grant it.

Two optional tasks. "Start when I sign in" writes the HKCU Run value,
which is the key Task Manager's Startup tab reads, so somebody who
changes their mind finds the switch where they will look for it. "Add to
PATH" is off by default and exists for pheme displays, which is the only
way to learn the monitor input values the display feature needs.

The PATH task carries a Check so a repeat install appends once rather
than twice, and an uninstall step that removes the entry without
disturbing the rest of the variable. Nothing removes the user's
config.toml.

The script cannot be compiled on the development machine -- there is no
iscc and no Windows -- so CI is its first compile.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 5: Release workflow

**Files:**
- Modify: `.github/workflows/release.yml`

**Interfaces:**
- Consumes: the metadata from Tasks 2 and 3, the script from Task 4, and `packaging/verify-*.sh` as the shape the CI steps mirror.
- Produces: a release carrying the tarball, the zip, the `.deb`, the `.rpm` and the Windows installer, each with a SHA256.

Read the existing file first. It already builds, packages a tarball and a zip, uploads artifacts, and publishes on a tag. You are adding steps, not rewriting it.

- [ ] **Step 1: Add the version guard**

As the first step of the `build` job after `checkout`, so a mismatched tag fails before anything is built:

```yaml
      # A tag of v0.2.0 against a crate at 0.1.0 would produce
      # pheme_0.1.0_amd64.deb inside a release called v0.2.0. Fail before
      # anything is built rather than publish files that disagree with
      # their own release name.
      - name: The tag must match the crate version
        if: startsWith(github.ref, 'refs/tags/') && runner.os == 'Linux'
        run: |
          v=$(cargo metadata --format-version 1 --no-deps \
              | jq -r '.packages[] | select(.name=="pheme-app") | .version')
          test "v$v" = "$GITHUB_REF_NAME" \
            || { echo "tag $GITHUB_REF_NAME does not match crate version $v"; exit 1; }
```

- [ ] **Step 2: Build the Linux packages**

After the existing `Build (Linux)` step:

```yaml
      - name: Build the Linux packages
        if: runner.os == 'Linux'
        run: |
          cargo install cargo-deb cargo-generate-rpm --locked
          # --no-build reuses the binary the previous step produced rather
          # than compiling the workspace a second time.
          cargo deb -p pheme-app --no-build --no-strip
          cargo generate-rpm -p crates/pheme-app
          cp target/debian/*.deb target/generate-rpm/*.rpm .
```

- [ ] **Step 3: Install what was built, and run it**

This is the step the whole sub-project exists to be checked by. Straight after Step 2:

```yaml
      # The only real test this sub-project has. A dependency that was
      # never declared fails the install; one declared wrongly fails the
      # run. Neither is visible from reading the manifest.
      - name: The .deb installs and runs
        if: runner.os == 'Linux'
        run: |
          echo "--- Depends ---"
          dpkg-deb -f ./pheme_*_amd64.deb Depends
          sudo apt-get install -y ./pheme_*_amd64.deb
          pheme --version
          for p in /usr/bin/pheme \
                   /usr/lib/udev/rules.d/80-pheme.rules \
                   /usr/lib/modules-load.d/pheme.conf \
                   /usr/lib/systemd/user/pheme.service \
                   /usr/share/applications/pheme.desktop \
                   /usr/share/icons/hicolor/32x32/apps/pheme.png; do
            test -f "$p" || { echo "MISSING: $p"; exit 1; }
          done
          # postinst runs again on upgrade; it must not fail the second time.
          sudo apt-get install -y --reinstall ./pheme_*_amd64.deb
          pheme --version
          sudo apt-get remove -y pheme
          test ! -e /usr/bin/pheme || { echo "remove left the binary"; exit 1; }

      - name: The .rpm installs and runs
        if: runner.os == 'Linux'
        run: |
          docker run --rm -v "$PWD:/w" fedora:latest sh -euxc '
            echo "--- Requires ---"
            rpm -qp --requires /w/pheme-*.x86_64.rpm
            dnf install -y /w/pheme-*.x86_64.rpm
            pheme --version
            test -f /usr/lib/systemd/user/pheme.service
            # post_install_script re-runs on an upgrade and must not fail
            # there, the same property the .deb step checks for postinst.
            dnf reinstall -y /w/pheme-*.x86_64.rpm
            pheme --version
          '
```

- [ ] **Step 4: Build and exercise the Windows installer**

After the existing `Package (Windows)` step:

```yaml
      - name: Build the Windows installer
        if: runner.os == 'Windows'
        shell: pwsh
        run: |
          $iscc = "${env:ProgramFiles(x86)}\Inno Setup 6\ISCC.exe"
          # The runner image ships Inno Setup. Install it if a future image
          # stops doing so, rather than fail on a missing tool.
          if (-not (Test-Path $iscc)) { choco install innosetup -y --no-progress }
          if (-not (Test-Path $iscc)) { throw "Inno Setup is not available at $iscc" }
          # The script uses x64compatible, which needs Inno Setup 6.3 or
          # newer. If a compile fails on an unknown identifier, the runner's
          # Inno Setup is older than that and this is the line to look at.
          $version = ((cargo metadata --format-version 1 --no-deps | ConvertFrom-Json).packages |
            Where-Object { $_.name -eq 'pheme-app' }).version
          & $iscc "/DAppVersion=$version" packaging\windows\pheme.iss
          if ($LASTEXITCODE -ne 0) { throw "iscc failed" }
          Copy-Item packaging\windows\Output\*.exe .

      - name: The installer installs, runs and uninstalls
        if: runner.os == 'Windows'
        shell: pwsh
        run: |
          $setup = (Get-ChildItem pheme-*-setup.exe)[0].FullName
          Start-Process -Wait $setup -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/TASKS=startup,addtopath'
          # Again, over the top, with the PATH task ticked both times. The
          # Check guard in the script is the only thing stopping the
          # directory being appended twice, and nothing else exercises it.
          Start-Process -Wait $setup -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/TASKS=startup,addtopath'
          $app = "$env:LOCALAPPDATA\Programs\Pheme"
          if (-not (Test-Path "$app\pheme.exe")) { throw "pheme.exe was not installed" }
          & "$app\pheme.exe" --version
          if ($LASTEXITCODE -ne 0) { throw "the installed binary does not run" }
          $run = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
                   -Name Pheme -ErrorAction SilentlyContinue
          if (-not $run) { throw "the startup task did not write its Run value" }
          $path = (Get-ItemProperty 'HKCU:\Environment' -Name Path).Path
          $hits = ($path -split ';' | Where-Object { $_ -eq $app }).Count
          if ($hits -ne 1) { throw "PATH holds the directory $hits times, expected once" }
          Start-Process -Wait "$app\unins000.exe" -ArgumentList '/VERYSILENT'
          if (Test-Path "$app\pheme.exe") { throw "uninstall left the binary behind" }
          $run = Get-ItemProperty 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' `
                   -Name Pheme -ErrorAction SilentlyContinue
          if ($run) { throw "uninstall left the Run value behind" }
          $path = (Get-ItemProperty 'HKCU:\Environment' -Name Path).Path
          if ($path -split ';' -contains $app) { throw "uninstall left the PATH entry behind" }
```

- [ ] **Step 5: Publish the new artifacts**

The Linux `Package` step gains a checksum for each package, and the Windows one for the installer. Extend the existing `sha256sum` and `Get-FileHash` steps rather than adding new ones, then widen the upload glob so the packages travel with the tarball:

```yaml
      - uses: actions/upload-artifact@v4
        with:
          name: pheme-${{ matrix.name }}
          path: |
            pheme-${{ matrix.name }}.*
            pheme_*.deb*
            pheme-*.rpm*
            pheme-*-setup.exe*
          if-no-files-found: error
```

The `publish` job needs no change: it already attaches every file it downloads.

- [ ] **Step 6: Check the workflow parses**

Run: `python3 -c "import yaml,sys; yaml.safe_load(open('.github/workflows/release.yml')); print('yaml ok')"`
Expected: `yaml ok`. A YAML error here is only discovered when a release is cut, which is the worst moment to discover it.

- [ ] **Step 7: Commit**

```bash
git add .github/workflows/release.yml
git commit -F - <<'EOF'
Update: build, verify and publish the packages

The release now carries a .deb, an .rpm and a Windows installer beside
the tarball and the zip it already produced.

Each is installed and run in the same job that built it. That is the
only real test this sub-project has: a dependency that was never
declared fails the install, one declared wrongly fails the run, and
neither is visible from reading the manifest. The .deb is also installed
a second time over itself, because postinst runs again on upgrade and
must not fail there; the Windows installer is uninstalled again and
checked for the Run value it should have removed.

A tag that disagrees with the crate version fails the job before
anything is built, rather than publishing pheme_0.1.0_amd64.deb inside a
release called v0.2.0.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 6: Documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/testing.md`

**Interfaces:**
- Consumes: everything.
- Produces: nothing.

- [ ] **Step 1: Rewrite the Linux runtime dependency section**

`README.md`'s `### Linux runtime dependencies` (around line 42) currently opens by telling people to install GTK and appindicator by hand. That is now the tarball's story, not everybody's. Replace the section with one that leads with the packages:

```markdown
## Installing

### Ubuntu

```bash
sudo apt install ./pheme_0.1.0_amd64.deb
```

The package declares what it needs, so apt installs GTK 3, PipeWire and
the AppIndicator library for you. It also installs the udev rule and
loads the kernel modules, which means `pheme setup` is not needed — it
is there for people installing from the tarball.

**This is an Ubuntu package, not a Debian one.** `cargo-deb` derives the
dependency names from the machine that builds it, and Ubuntu 24.04
renamed a number of libraries during the 64-bit `time_t` transition:
`libgtk-3-0` became `libgtk-3-0t64`, and others with it. Debian does not
provide those names, so this file will not satisfy on Debian 12 however
new the glibc there is. Debian users want the tarball, or their own
`cargo deb` run, which produces a package naming the libraries their
release actually ships. Either way, what a given build asks for is:

```bash
dpkg-deb -f pheme_*_amd64.deb Depends
```

### Fedora

```bash
sudo dnf install ./pheme-0.1.0-1.x86_64.rpm
```

### Windows

Run the installer. It needs no administrator: it installs into your own
profile, creates no service and writes nothing outside it.

Windows will warn that it does not recognise the program, because the
installer is not signed — a code-signing certificate is a recurring cost
this project does not carry. Choose **More info**, then **Run anyway**.
The SHA256 published beside the download is what you can check first, and
it is worth checking.

### From the tarball

The tarball is the binary and nothing else, so its dependencies are
yours to install:

```bash
# Debian/Ubuntu
sudo apt install libgtk-3-0 libayatana-appindicator3-1
# Arch
sudo pacman -S gtk3 libayatana-appindicator
# Fedora
sudo dnf install gtk3 libayatana-appindicator-gtk3
```

`pheme` links GTK 3 directly, so it has to be present to run the binary
**at all** — every subcommand, not only the tray and the window. Then run
`sudo pheme setup` once, which installs the udev rule and loads the
modules the packages would have handled.

### Starting with the session

On Windows, tick "Start Pheme when I sign in" during installation, or
find Pheme in Task Manager's Startup tab afterwards.

On Linux, the packages install a systemd user unit, disabled:

```bash
systemctl --user enable --now pheme
```
```

Keep the existing paragraph about GNOME and the AppIndicator extension —
it is still true and it still surprises people — and keep the paragraph
about the `-dev` packages needed to build from source.

- [ ] **Step 2: Add the manual test rows**

In `docs/testing.md`, a new section after the sub-project 7 one:

```markdown
## Packaging (sub-project 8)

CI installs both packages and the Windows installer on every release
build, so these rows cover what CI cannot see: a real desktop session, a
reboot, and what a person actually meets.

| # | Action | Pass |
|---|---|---|
| F1 | `apt install ./pheme_*.deb` on an Ubuntu with no GTK installed | apt pulls GTK and the AppIndicator library; `pheme --version` runs |
| F2 | `dnf install ./pheme-*.rpm` on a clean Fedora | the same |
| F3 | After F1, without rebooting, `pheme displays` | `/dev/i2c-*` is readable; no permission error |
| F4 | After F1, `systemctl --user enable --now pheme`, then sign out and in | the tray icon or the window appears |
| F5 | Open the application menu | Pheme is listed, with its icon |
| F6 | `apt remove pheme`, then `apt purge pheme` | the binary, the unit, the udev rule and the desktop entry are gone; `~/.config/pheme/config.toml` is untouched |
| F7 | Run the Windows installer as a standard user | no UAC prompt; it completes |
| F8 | Tick "Start Pheme when I sign in", then sign out and in | pheme is running, and is listed in Task Manager > Startup |
| F9 | Tick "Add Pheme to PATH", open a **new** terminal, run `pheme displays` | the command is found |
| F10 | Install again over the top, with the PATH task ticked both times | PATH contains the directory once, not twice |
| F11 | Uninstall on Windows | the directory, the shortcut, the Run value and the PATH entry are gone; `%APPDATA%\pheme\config.toml` is untouched |
| F12 | Download the installer in a browser | SmartScreen warns; the README's steps get past it; the published SHA256 matches |
```

- [ ] **Step 3: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git status --porcelain Cargo.lock
```

The last command must print nothing.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/testing.md
git commit -F - <<'EOF'
Update: document installing from a package

The README now leads with the packages on each platform and keeps the
hand-installed library list where it belongs, under the tarball. The
packages declare their dependencies, install the udev rule and load the
modules, so `pheme setup` is the tarball's step rather than everybody's.

It also says plainly that the Windows installer is unsigned, that
SmartScreen will warn, and how to get past it, with the published SHA256
as the thing a person can actually check. No euphemism, and no advice to
turn a security feature off.

F1 to F12 cover what CI cannot: a real desktop session, a reboot, PATH
in a new terminal, and what a person meets when they download the
installer in a browser.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```
