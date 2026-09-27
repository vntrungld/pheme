# Sub-project 8 — Packaging

Date: 2026-09-26
Overall architecture: `2026-09-21-pheme-architecture-design.md`
Previous sub-project: `2026-09-26-display-switching-design.md`
Expected outcome: pheme installs with one command on Debian, Ubuntu and
Fedora, and with one double-click on Windows, without the person reading
a list of libraries to install first.

## 1. Scope

In:

- A `.deb` and an `.rpm`, both built in CI, both declaring their runtime
  dependencies so the package manager installs them.
- The udev rule, the `modules-load.d` entry, a systemd `--user` unit and a
  desktop entry, installed to their proper places by the packages.
- A Windows installer built with Inno Setup: no administrator rights, a
  Start Menu shortcut, an opt-in "start when I sign in", and an opt-in
  PATH entry.
- CI that builds all of it, **installs it, and runs it** — the only
  meaningful test this sub-project has.

Out:

- **Code signing.** An Authenticode certificate costs money annually and
  an EV one needs a hardware token. The README says plainly that
  SmartScreen will warn, how to proceed, and publishes a SHA256 — which is
  the thing a person can actually verify. §9.
- **AppImage, Flatpak, Snap, winget, Homebrew, the AUR.** Each is a
  distribution channel with its own review process and its own metadata to
  keep correct. Two native package formats and one installer is the whole
  of this sub-project.
- **Auto-update.** Nothing here phones home or replaces itself.
- **A build without GTK.** Both machines running pheme have a desktop by
  definition — the server is the machine a person sits at and the client
  is the machine they look at — and GTK 3 is present on every desktop
  Linux. A cargo feature would add a build combination to test that
  nobody runs. The packages declare GTK as a dependency instead, which is
  what solves the problem a person actually has (§3.2).
- **An application icon for Windows.** The repository's only artwork is a
  pair of 32×32 tray PNGs. The installer and the executable use the system
  default rather than have this sub-project invent artwork.

## 2. Why packages rather than better instructions

`README.md` currently tells a person to install `libgtk-3-0` and
`libayatana-appindicator3-1` before running pheme at all, because the
binary links GTK directly — 74 shared libraries, including `libgtk-3.so.0`,
`libgdk-3.so.0` and `libpipewire-0.3.so.0`. That list is written by hand,
which means it is wrong the first time a dependency changes and nobody
notices until somebody's `pheme` dies at load time.

`cargo-deb` resolves this properly. Its default for `depends` is `$auto`:
it runs `ldd` over the built binary, takes each soname, and asks `dpkg`
which package provides it. The `Depends:` field is then derived from the
binary that was actually built, not from a list somebody remembered to
update. `cargo-generate-rpm` does the equivalent — `AutoReqMode::Auto`
finds no `/usr/lib/rpm/find-requires` on an Ubuntu runner and falls back
to its built-in ELF reader, emitting soname requirements in the form RPM
resolves against providers.

So both packages are built from one Ubuntu runner and each is correct for
its own ecosystem.

**Twelve dependencies `$auto` cannot see.** Automatic detection reads the
ELF header, so it finds only what is in `DT_NEEDED` — and this binary
`dlopen`s twelve libraries it never links. `readelf -d` lists the twelve
that are linked; `strings` over the same binary finds twelve more versioned
sonames that are not:

| soname | opened by |
|---|---|
| `libayatana-appindicator3.so.1` | the tray |
| `libEGL.so.1`, `libGL.so.1` | glutin, to make a GL context |
| `libX11.so.6`, `libX11-xcb.so.1`, `libXcursor.so.1`, `libXi.so.6`, `libXrender.so.1` | winit's X11 backend, via `x11-dl` |
| `libxkbcommon.so.0`, `libxkbcommon-x11.so.0` | winit's keymap handling |
| `libwayland-client.so.0`, `libwayland-egl.so.1` | winit's Wayland backend |

All twelve are declared by hand. Four of them —
`libEGL.so.1`, `libGL.so.1`, `libX11-xcb.so.1` and
`libxkbcommon-x11.so.0` — are absent from a clean `ubuntu:24.04` that has
only the rest of the `Depends` satisfied, so leaving them out is not
theoretical: `apt install` succeeds, `pheme --version` prints, `pheme
server` and `pheme client` work, and `pheme` with no subcommand — what the
desktop entry and the systemd unit both launch — cannot create a GL
context. The remaining eight arrive transitively through GTK 3 today, and
are declared anyway, because a transitive accident is not a dependency.

`pheme --version` cannot prove any of this, because clap exits before the
first `dlopen`. What proves it is `packaging/check-dlopen-sonames.sh`,
which asks `ldconfig` for each soname by name inside the same clean
container the package was just installed in. §8.

## 3. The Linux packages

### 3.1 What they install

| Path | Content |
|---|---|
| `/usr/bin/pheme` | the binary |
| `/usr/lib/udev/rules.d/80-pheme.rules` | `setup::UDEV_RULE`, verbatim |
| `/usr/lib/modules-load.d/pheme.conf` | `setup::MODULES_LOAD`, verbatim |
| `/usr/lib/systemd/user/pheme.service` | the user unit, **not enabled** (§3.4) |
| `/usr/share/applications/pheme.desktop` | the application menu entry |
| `/usr/share/icons/hicolor/32x32/apps/pheme.png` | `assets/tray-connected.png` |
| `/usr/share/doc/pheme/README.md` | |
| `/usr/share/doc/pheme/testing.md` | |
| `/usr/share/doc/pheme/LICENSE` | the `.rpm`; the `.deb` puts it at `/usr/share/doc/pheme/copyright`, which is what Debian policy asks for |

The udev rule and the modules file are the same text `pheme setup` writes.
They are generated from the constants at build time rather than copied by
hand, so the two cannot drift: a build script or a test asserts that the
file in `packaging/` equals `setup::UDEV_RULE`. `pheme setup` remains, for
people who installed from the tarball.

### 3.2 Dependencies

`cargo-deb` is left on its default `$auto` and given the twelve `dlopen`'d
libraries of §2 as Ubuntu package names:

```toml
depends = "$auto, libayatana-appindicator3-1 | libappindicator3-1, \
libgl1, libegl1, libx11-6, libx11-xcb1, libxcursor1, libxi6, libxrender1, \
libxkbcommon0, libxkbcommon-x11-0, libwayland-client0, libwayland-egl1"
```

`cargo-generate-rpm` gets the equivalent, expressed as the sonames rather
than package names, because what provides them differs between Fedora and
openSUSE while the sonames do not:

```toml
[package.metadata.generate-rpm.requires]
"libayatana-appindicator3.so.1()(64bit)" = "*"
"libEGL.so.1()(64bit)" = "*"
# ...and one line per soname in §2's table.
```

Both the X11 and the Wayland libraries are declared although a given
machine runs only one display server. `Depends` has no way to say "X11 or
Wayland", each package is small, and the alternative is a dependency list
that is honest on half the machines.

`auto-req` is left at its default, which is what selects the built-in ELF
reader on a host with no `find-requires`.

The tray degrades cleanly when the library is missing — sub-project 6 made
`Tray::new` catch that failure and open the window instead — so this is a
`Depends`, not a hard requirement the program cannot start without. It is
declared because a tray that silently never appears is a worse first
experience than an extra megabyte of download.

### 3.3 Maintainer scripts

`postinst` does what the root path of `pheme setup` does, in the same
order and for the same reasons:

```sh
modprobe uinput || true
modprobe i2c-dev || true
udevadm control --reload || true
udevadm trigger --name-match=uinput || true
udevadm trigger --subsystem-match=i2c-dev || true
```

Each is tolerant of failure: a container has no `/sys` to trigger against
and a package install must not fail there. The ordering is load-bearing
and matches `setup.rs`'s own comment — `modprobe` creates the device nodes,
the reload picks up the rule file just installed, and the triggers
reprocess the nodes against it. Without the triggers the rule applies to
nothing until a reboot.

`postinst` does **not** add anybody to a group. `pheme setup` adds the
invoking user to `input` because pheme cannot work at all without it, but
a package installs system-wide with no invoking user to speak of, and
`TAG+="uaccess"` covers the logged-in user on any system with systemd-logind.
The README says what to do on a system where it does not.

### 3.4 The systemd user unit

```ini
[Unit]
Description=Pheme keyboard, mouse and audio sharing
PartOf=graphical-session.target

[Service]
ExecStart=/usr/bin/pheme
Restart=on-failure
RestartSec=2

[Install]
WantedBy=graphical-session.target
```

It runs `pheme` with no subcommand — the front-end, which supervises the
server or client child and owns the tray, matching what the Windows
installer starts. It is installed but **not enabled**: a user unit belongs
to a person, not to a machine, and `cargo-deb`'s `systemd-units` support
is for system units only — it has no notion of a user unit at all, which
was checked rather than assumed. A person turns it on with

```
systemctl --user enable --now pheme
```

`Restart=on-failure` and not `always`: the front-end exiting because
somebody chose Quit is not a failure to be undone.

### 3.5 The desktop entry

```ini
[Desktop Entry]
Type=Application
Name=Pheme
Comment=Share one keyboard, mouse, clipboard and audio between two machines
Exec=pheme
Icon=pheme
Terminal=false
Categories=Utility;Network;RemoteAccess;
```

This is the application-menu entry, not an autostart file. Autostart is
the systemd unit above; shipping both a `.desktop` in `/etc/xdg/autostart`
and a unit would give one machine two ways to start the same program and
no way to tell which did.

## 4. The Windows installer

Inno Setup, script at `packaging/windows/pheme.iss`, version passed in by
CI with `/DAppVersion=`.

```
PrivilegesRequired=lowest
DefaultDirName={autopf}\Pheme
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
OutputBaseFilename=pheme-{#AppVersion}-x86_64-windows-setup
```

`PrivilegesRequired=lowest` is the important line. With it `{autopf}`
resolves to `%LOCALAPPDATA%\Programs`, no UAC prompt appears, and the
installer needs no administrator. Pheme has never needed administrator on
Windows — it creates no service and writes nothing outside the user's own
profile — and an installer that asks for it trains people to grant it.

Two optional tasks, both off by default except the first:

- **"Start Pheme when I sign in"**, checked by default, writes
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Pheme` with
  `uninsdeletevalue`. That is the key Task Manager's Startup tab reads, so
  a person who changes their mind finds the switch where they look first.
- **"Add Pheme to PATH"**, unchecked, appends `{app}` to the user's
  `Environment\Path`. This is for `pheme displays`, which is the only way
  to learn the monitor input values sub-project 7 needs, and `pheme pair`
  from a terminal.

The PATH task needs Pascal helpers: one `Check` so a repeated install does
not append twice, and an uninstall step that removes the entry without
disturbing the rest of the variable. It is the most delicate part of the
installer and the reason the task is opt-in.

Uninstall removes the program directory, the Start Menu shortcut, the Run
value and the PATH entry. It leaves `%APPDATA%\pheme\config.toml` alone:
an uninstaller that deletes a person's configuration is an uninstaller
people learn to fear.

## 5. CI

`release.yml` gains package steps on both legs and keeps the tarball and
zip it already produces — somebody who wants a binary without a package
manager still gets one.

**Linux**, after the existing `cargo build --release --locked`:

```
cargo install cargo-deb cargo-generate-rpm --locked
cargo deb -p pheme-app --no-build --no-strip
cargo generate-rpm -p crates/pheme-app
```

`--no-build` reuses the release binary the previous step produced rather
than compiling the workspace a second time.

**Windows**, after its build: run `iscc` on the script. The runner image
ships Inno Setup; the step installs it with `choco install innosetup -y`
when `iscc` is not on PATH, so it is correct whether or not a future
runner image drops it.

**A version guard**, on tag pushes only: the tag must equal the crate
version, or the job fails before anything is published. Without it a tag
of `v0.2.0` produces `pheme_0.1.0_amd64.deb` and the release carries files
that disagree with their own release name.

## 6. Repository metadata

Two things both package formats read are currently wrong or absent, and
both are fixed here:

- `Cargo.toml`'s `repository` says `https://github.com/trungld/pheme`. The
  remote is `vntrungld/pheme`. The wrong URL is embedded in both packages.
- `pheme-app` has no `description`. `.deb` and `.rpm` both require one.

A `maintainer` is also required by `cargo-deb`. It takes the git identity
already recorded in every commit in this repository.

## 7. What this does not do

Listed in §1. The one worth repeating: nothing here is signed, so Windows
will warn on download and on first run.

## 8. Testing

This sub-project has almost nothing a unit test can reach. What it has
instead is better, and it runs in CI on every release build:

- **Ubuntu:** `sudo apt-get install -y ./pheme_*_amd64.deb` inside a clean
  `ubuntu:24.04` container, then `pheme --version`, then
  `packaging/check-dlopen-sonames.sh`. A missing dependency fails the
  install; a wrong one fails the run; an undeclared `dlopen`'d one fails
  only the soname check, which is why that check exists. A clean container
  and not the runner: the runner has already installed every `-dev`
  package, so nothing about dependency closure can fail there.
- **Fedora:** the same three, inside `docker run --rm fedora:latest`, with
  `dnf install -y`. The runner has Docker and the image is the only place
  an `.rpm` can honestly be tested from an Ubuntu host.
- **File placement:** `dpkg -c` and `rpm -qlp` list the archive contents;
  assert every path in §3.1 is present, once, with the right mode.
- **Windows:** run the installer with `/VERYSILENT /SUPPRESSMSGBOXES`,
  assert `pheme.exe` exists and `--version` runs, assert the `Run` value
  exists, then run the uninstaller with `/VERYSILENT` and assert the
  directory and the `Run` value are both gone.

Unit-testable, and tested:

- the udev rule and modules file in `packaging/` are byte-identical to
  `setup::UDEV_RULE` and `setup::MODULES_LOAD`;
- the `.desktop` file parses and its `Exec` names a binary the package
  installs;
- the systemd unit's `ExecStart` path matches where the package puts the
  binary.

Each of those three is a file that can silently drift from the code it
mirrors, which is exactly the class of defect a test can catch cheaply.

## 9. The SmartScreen warning

The README gains a short, honest section: the installer is not signed,
Windows will show "Windows protected your PC", the way through is *More
info* then *Run anyway*, and the SHA256 published beside the download is
what a person can check before doing so. No euphemism and no advice to
disable a security feature.

## 10. Manual test matrix (added to `docs/testing.md`)

| # | Action | Pass |
|---|---|---|
| F1 | `apt install ./pheme_*.deb` on a clean Ubuntu with no GTK | apt pulls GTK and appindicator; `pheme --version` runs |
| F2 | `dnf install ./pheme-*.rpm` on a clean Fedora | the same |
| F3 | After F1, without a reboot, `pheme displays` | `/dev/i2c-*` is readable; the command does not report a permission error |
| F4 | After F1, `systemctl --user enable --now pheme` then log out and back in | the tray icon or the window appears |
| F5 | Pheme appears in the application menu with its icon | |
| F6 | `apt remove pheme` then `apt purge pheme` | the binary, the unit, the udev rule and the desktop entry are gone; `~/.config/pheme/config.toml` remains |
| F7 | Run the Windows installer as a standard user | no UAC prompt; it completes |
| F8 | Tick "start when I sign in", sign out and in | pheme is running; the entry is listed in Task Manager > Startup |
| F9 | Tick "Add to PATH", open a new terminal, `pheme displays` | the command is found |
| F10 | Install twice in a row with the PATH task ticked | PATH contains the directory once, not twice |
| F11 | Uninstall on Windows | the directory, the shortcut, the Run value and the PATH entry are gone; `%APPDATA%\pheme\config.toml` remains |
| F12 | Download the installer in a browser | SmartScreen warns; the README's steps get past it; the published SHA256 matches |

## 11. Definition of done

- `.deb`, `.rpm`, Windows installer, tarball and zip all attached to a
  release, each with a SHA256.
- CI installs the `.deb` on Ubuntu and the `.rpm` on Fedora and runs the
  binary from both; CI installs and uninstalls on Windows.
- The packaged udev rule and modules file are pinned to the constants by a
  test.
- A tag whose version disagrees with the crate fails the release.
- `README.md` documents installing from a package on each platform, the
  systemd user unit, and the SmartScreen warning; `docs/testing.md`
  carries F1–F12.
- `cargo fmt`, `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` clean on both CI legs.

## 12. Known risks

- **`$auto` sees only what is linked.** All twelve `dlopen` dependencies
  are declared by hand (§2), and a hand-written list is wrong as soon as a
  crate upgrade adds a thirteenth. Installing and running the package does
  not catch that — `pheme --version` exits inside clap before the first
  `dlopen`, which is exactly how the first four went undeclared through six
  task reviews. `packaging/check-dlopen-sonames.sh` is what catches it: it
  asks `ldconfig` for each soname inside the clean container, it runs on
  both legs in CI and in both verify scripts, and it fails rather than
  passing quietly. It carries the `readelf -d` and `strings` commands that
  regenerate its own list.
- **The PATH task.** Appending to a user's `Path` from an installer is a
  well-known source of duplicated and truncated variables. It is opt-in,
  it has a `Check` against duplication and an explicit uninstall step, and
  F9, F10 and F11 exercise it.
- **`cargo-deb` and `cargo-generate-rpm` are build-time tools installed
  from crates.io in CI**, so a release build depends on crates.io being up
  and on those tools not breaking. `--locked` is used where the tools
  support it.
- **Fedora testing runs in a container**, which is not the same as a real
  Fedora desktop: it proves dependency resolution and that the binary
  starts, not that the tray works there.
- **No package is tested on a system older than the CI runner.** A `.deb`
  built against Ubuntu's current GTK will declare a version floor that an
  older Debian cannot satisfy. That is inherent to building one package on
  one runner, and the README states which releases it is built against.
