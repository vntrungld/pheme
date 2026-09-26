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
    assert_eq!(
        read("packaging/linux/80-pheme.rules"),
        pheme_app::setup::UDEV_RULE
    );
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
        assert!(
            desktop.lines().any(|l| l == key),
            "no `{key}` line in:\n{desktop}"
        );
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
/// package. Break it by adding `WantedBy=default.target`, which would make
/// `systemctl --user enable` hook it into a non-graphical target and start
/// a GUI front-end on a machine with no session to draw into.
#[test]
fn the_unit_belongs_to_the_graphical_session() {
    let unit = read("packaging/linux/pheme.service");
    assert!(unit
        .lines()
        .any(|l| l == "WantedBy=graphical-session.target"));
    assert!(unit.lines().any(|l| l == "PartOf=graphical-session.target"));
}
