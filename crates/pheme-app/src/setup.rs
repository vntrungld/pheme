//! `pheme setup`: one-time OS prerequisites.

pub const UDEV_RULE: &str = "# Pheme: allow members of the input group to create virtual input devices\nKERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"\n# Pheme: allow the logged-in user to speak DDC/CI to monitors over i2c\nKERNEL==\"i2c-[0-9]*\", MODE=\"0660\", GROUP=\"i2c\", TAG+=\"uaccess\"\n";

/// Makes systemd load `uinput` (virtual input devices) and `i2c-dev`
/// (DDC/CI monitor control) at boot; `modprobe` alone does not survive a
/// reboot on most distributions.
pub const MODULES_LOAD: &str = "uinput\ni2c-dev\n";

#[cfg(target_os = "linux")]
pub fn run() -> anyhow::Result<()> {
    // Imported here rather than at module scope: the Windows and fallback `run`
    // bodies below do not use it, and an unused import fails `-D warnings` there.
    use anyhow::Context;
    use std::path::Path;
    use std::process::Command;

    let rule_path = Path::new("/etc/udev/rules.d/80-pheme.rules");
    let modules_path = Path::new("/etc/modules-load.d/pheme.conf");
    let user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let is_root = unsafe { libc_geteuid() } == 0;
    println!(
        "Discovery uses mDNS on UDP port 5353. If `pheme discover` finds nothing, \
         allow that port through the firewall, or put the server's address in `connect`."
    );
    if !is_root {
        println!("Run the following as root (or re-run `sudo pheme setup`):");
        println!();
        print!("{}", recipe(rule_path, modules_path, &user));
        return Ok(());
    }
    std::fs::write(rule_path, UDEV_RULE)
        .with_context(|| format!("writing {}", rule_path.display()))?;
    println!("Wrote {}", rule_path.display());
    let mut ok = true;
    // Load the modules before touching udev. `udevadm trigger` resolves its
    // matches through sysfs, so on a machine where a module has never been
    // loaded there is nothing there yet to match, and the trigger for it
    // either fails outright (uinput: "Failed to open the device 'uinput':
    // Invalid argument") or, worse, silently matches nothing (i2c-dev: the
    // rule's `KERNEL=="i2c-[0-9]*"` has no /dev/i2c-* nodes to apply to
    // until the module creates them) -- which is exactly the machine this
    // command exists to set up, and exactly the machine `pheme displays`
    // would otherwise still find nothing on afterwards.
    ok &= run_step("modprobe uinput", Command::new("modprobe").arg("uinput"));
    ok &= run_step("modprobe i2c-dev", Command::new("modprobe").arg("i2c-dev"));
    ok &= run_step(
        "udevadm control --reload",
        Command::new("udevadm").args(["control", "--reload"]),
    );
    ok &= run_step(
        "udevadm trigger --name-match=uinput",
        Command::new("udevadm").args(["trigger", "--name-match=uinput"]),
    );
    // uinput is one device matched by /dev name; the i2c buses are a whole
    // subsystem (i2c-0, i2c-1, ...) whose count and names vary by machine,
    // so they are matched by subsystem rather than by name. `SUBSYSTEM==
    // "i2c-dev"` is exactly what the i2c-dev module tags the bus character
    // devices with (confirmed against /sys/class/i2c-dev on this machine),
    // so it re-triggers precisely the nodes the new udev rule applies to,
    // without also matching i2c adapters or muxes that carry no character
    // device at all.
    ok &= run_step(
        "udevadm trigger --subsystem-match=i2c-dev",
        Command::new("udevadm").args(["trigger", "--subsystem-match=i2c-dev"]),
    );
    match std::fs::write(modules_path, MODULES_LOAD) {
        Ok(()) => println!(
            "Wrote {} (uinput and i2c-dev load at boot)",
            modules_path.display()
        ),
        Err(e) => {
            eprintln!(
                "warning: writing {} failed: {e}; uinput and i2c-dev will need \
                 `modprobe uinput` / `modprobe i2c-dev` after each boot",
                modules_path.display()
            );
            ok = false;
        }
    }
    if !user.is_empty() && user != "root" {
        let st = Command::new("usermod")
            .args(["-aG", "input", &user])
            .status()
            .with_context(|| format!("running usermod -aG input {user}"))?;
        if st.success() {
            println!("Added {user} to the input group. Log out and back in for it to apply.");
        } else {
            let code = st
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            anyhow::bail!(
                "usermod -aG input {user} failed (exit {code}); add the user to the input group manually"
            );
        }
    }
    if ok {
        println!("Setup complete.");
    } else {
        println!(
            "Setup finished with warnings (see above); reboot or reload udev manually if /dev/uinput stays inaccessible."
        );
    }
    // Unlike `input` above, `pheme setup` does not add the user to `i2c`
    // itself -- it only prints the command. The two groups are handled
    // differently on purpose, not by oversight: `input` is required for
    // pheme to work at all (no virtual keyboard or mouse without it), so
    // adding it automatically is the price of the program running at all.
    // `i2c` grants raw access to every I2C bus on the machine, not just
    // the ones behind a monitor, for a feature that is optional and that
    // many monitors do not even support. The udev rule's `TAG+="uaccess"`
    // already covers the logged-in user on most systems without any group
    // change, so this is a fallback for systems where it does not, not the
    // primary mechanism -- and enabling it is the person's call, not
    // this command's.
    println!(
        "If DDC/CI still fails, add yourself to the i2c group and log in again:\n  \
         usermod -aG i2c {user}"
    );
    Ok(())
}

/// The copy-and-paste form of everything the root path below does, for
/// somebody who ran `pheme setup` without root.
///
/// Built from the same constants and listing the same commands, because a
/// recipe that has drifted from the real thing is worse than no recipe at
/// all: it leaves a machine that looks set up and is not. The two steps
/// that were missing when this was a hand-written string are the ones with
/// no symptom until a reboot -- `udevadm trigger --subsystem-match=i2c-dev`,
/// without which the new rule never reaches the `/dev/i2c-*` nodes that
/// already exist, and the `i2c` group fallback for systems `uaccess` does
/// not cover.
#[cfg(target_os = "linux")]
fn recipe(rule_path: &std::path::Path, modules_path: &std::path::Path, user: &str) -> String {
    let rule = rule_path.display();
    let modules = modules_path.display();
    // The file contents come from the constants rather than being written
    // out again here, so the two cannot say different things. The heredoc
    // bodies are the one part that must not be indented.
    let commands = [
        format!("cat > {rule} <<'EOF'\n{UDEV_RULE}EOF"),
        "modprobe uinput && modprobe i2c-dev".to_string(),
        format!("cat > {modules} <<'EOF'\n{MODULES_LOAD}EOF"),
        "udevadm control --reload".to_string(),
        // Not optional, and the one with no symptom until a reboot: a
        // reload alone does not re-apply the new rule to the /dev/i2c-*
        // nodes that already exist.
        "udevadm trigger --name-match=uinput".to_string(),
        "udevadm trigger --subsystem-match=i2c-dev".to_string(),
        format!("usermod -aG input {user}"),
    ];
    let mut out = String::new();
    for c in commands {
        out.push_str("  ");
        out.push_str(&c);
        out.push('\n');
    }
    out.push_str(
        "\nThen log out and back in so the group change applies.\n\
         If DDC/CI still fails, add yourself to the i2c group as well:\n",
    );
    out.push_str(&format!("  usermod -aG i2c {user}\n"));
    out
}

/// Runs `cmd`, printing a `warning:` line to stderr and returning `false` on
/// spawn error or non-zero exit; returns `true` on success.
#[cfg(target_os = "linux")]
fn run_step(desc: &str, cmd: &mut std::process::Command) -> bool {
    match cmd.status() {
        Ok(st) if st.success() => true,
        Ok(st) => {
            let code = st
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "signal".to_string());
            eprintln!("warning: {desc} failed: exit {code}");
            false
        }
        Err(e) => {
            eprintln!("warning: {desc} failed: {e}");
            false
        }
    }
}

#[cfg(target_os = "linux")]
unsafe fn libc_geteuid() -> u32 {
    extern "C" {
        fn geteuid() -> u32;
    }
    geteuid()
}

#[cfg(target_os = "windows")]
pub fn run() -> anyhow::Result<()> {
    println!("Nothing to set up for keyboard/mouse sharing on Windows.");
    println!("Audio forwarding (a later release) will need VB-CABLE: https://vb-audio.com/Cable/");
    println!(
        "Discovery uses mDNS on UDP port 5353. If `pheme discover` finds nothing, \
         allow that port through the firewall, or put the server's address in `connect`."
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn run() -> anyhow::Result<()> {
    anyhow::bail!("setup is not implemented for this OS")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udev_rule_targets_uinput_for_the_input_group() {
        assert!(UDEV_RULE.contains("KERNEL==\"uinput\""));
        assert!(UDEV_RULE.contains("GROUP=\"input\""));
        assert!(UDEV_RULE.contains("uaccess"));
        assert!(UDEV_RULE.ends_with('\n'));
    }

    /// Break it by replacing rather than appending: uinput stops being
    /// loaded and virtual input devices stop working, which is the whole
    /// of sub-project 1.
    #[test]
    fn modules_load_entry_names_both_modules() {
        let lines: Vec<&str> = MODULES_LOAD.lines().collect();
        assert!(lines.contains(&"uinput"), "{MODULES_LOAD:?}");
        assert!(lines.contains(&"i2c-dev"), "{MODULES_LOAD:?}");
    }

    /// Break it by matching `KERNEL=="i2c*"`: that also matches the
    /// `i2c-dev` bus devices' parents and other i2c character devices this
    /// rule has no business relaxing.
    #[test]
    fn udev_rule_covers_the_i2c_buses() {
        assert!(UDEV_RULE.contains(r#"KERNEL=="i2c-[0-9]*""#), "{UDEV_RULE}");
        assert!(UDEV_RULE.contains(r#"GROUP="i2c""#), "{UDEV_RULE}");
        // uaccess is what makes this work without group membership, the
        // same way the uinput rule already does.
        assert_eq!(UDEV_RULE.matches(r#"TAG+="uaccess""#).count(), 2);
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod recipe_tests {
    use super::*;
    use std::path::Path;

    fn text() -> String {
        recipe(
            Path::new("/etc/udev/rules.d/80-pheme.rules"),
            Path::new("/etc/modules-load.d/pheme.conf"),
            "ada",
        )
    }

    /// The finding this exists for: the printed recipe reloaded udev and
    /// triggered uinput, but never re-triggered the i2c nodes, so somebody
    /// who pasted it exactly got the rule written, `i2c-dev` loaded, and
    /// the permissions on the existing `/dev/i2c-*` nodes unchanged until
    /// a reboot -- on the machine this command exists to set up.
    ///
    /// Break it by deleting the `udevadm trigger --subsystem-match=i2c-dev`
    /// line from `recipe`.
    #[test]
    fn the_recipe_retriggers_the_i2c_nodes() {
        let t = text();
        assert!(t.contains("udevadm control --reload"), "{t}");
        assert!(t.contains("udevadm trigger --name-match=uinput"), "{t}");
        assert!(
            t.contains("udevadm trigger --subsystem-match=i2c-dev"),
            "{t}"
        );
    }

    /// Break it by writing the module names out by hand again, the way
    /// this printed them before: `MODULES_LOAD` and the recipe then drift
    /// with nothing pinning them, which is how i2c-dev came to be in one
    /// and not the other.
    #[test]
    fn the_recipe_writes_the_modules_file_from_the_constant() {
        let t = text();
        assert!(t.contains(MODULES_LOAD), "{t}");
        assert!(t.contains(UDEV_RULE), "{t}");
        for m in MODULES_LOAD.lines() {
            assert!(
                t.contains(&format!("modprobe {m}")),
                "{m} is not loaded now: {t}"
            );
        }
    }

    /// The root path prints the `i2c` group fallback and explains why it
    /// does not run it; the recipe omitted it entirely, so the person most
    /// likely to need it -- one without root, on a system `uaccess` does
    /// not cover -- was the one not told.
    ///
    /// Break it by deleting the last two lines of `recipe`.
    #[test]
    fn the_recipe_offers_the_i2c_group_fallback() {
        let t = text();
        assert!(t.contains("usermod -aG input ada"), "{t}");
        assert!(t.contains("usermod -aG i2c ada"), "{t}");
    }
}
