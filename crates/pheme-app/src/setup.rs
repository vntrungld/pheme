//! `pheme setup`: one-time OS prerequisites.

pub const UDEV_RULE: &str = "# Pheme: allow members of the input group to create virtual input devices\nKERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"\n";

/// Makes systemd load the `uinput` module at boot (`modprobe uinput` alone does not
/// survive a reboot on most distributions).
pub const MODULES_LOAD: &str = "uinput\n";

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
    if !is_root {
        println!("Run the following as root (or re-run `sudo pheme setup`):");
        println!();
        println!("  cat > {} <<'EOF'\n{}EOF", rule_path.display(), UDEV_RULE);
        println!(
            "  modprobe uinput && echo uinput > {}",
            modules_path.display()
        );
        println!("  udevadm control --reload && udevadm trigger --name-match=uinput");
        println!("  usermod -aG input {user}");
        println!();
        println!("Then log out and back in so the group change applies.");
        return Ok(());
    }
    std::fs::write(rule_path, UDEV_RULE)
        .with_context(|| format!("writing {}", rule_path.display()))?;
    println!("Wrote {}", rule_path.display());
    let mut ok = true;
    // Load the module before touching udev. `udevadm trigger --name-match=uinput`
    // resolves the name through sysfs, so on a machine where uinput has never been
    // loaded there is no /sys/devices/virtual/misc/uinput to match and the trigger
    // fails with "Failed to open the device 'uinput': Invalid argument" — which is
    // exactly the machine this command exists to set up.
    ok &= run_step("modprobe uinput", Command::new("modprobe").arg("uinput"));
    ok &= run_step(
        "udevadm control --reload",
        Command::new("udevadm").args(["control", "--reload"]),
    );
    ok &= run_step(
        "udevadm trigger --name-match=uinput",
        Command::new("udevadm").args(["trigger", "--name-match=uinput"]),
    );
    match std::fs::write(modules_path, MODULES_LOAD) {
        Ok(()) => println!("Wrote {} (uinput loads at boot)", modules_path.display()),
        Err(e) => {
            eprintln!(
                "warning: writing {} failed: {e}; uinput will need `modprobe uinput` after each boot",
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
    Ok(())
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
    fn modules_load_entry_names_the_uinput_module() {
        assert_eq!(MODULES_LOAD, "uinput\n");
    }

    #[test]
    fn udev_rule_targets_uinput_for_the_input_group() {
        assert!(UDEV_RULE.contains("KERNEL==\"uinput\""));
        assert!(UDEV_RULE.contains("GROUP=\"input\""));
        assert!(UDEV_RULE.contains("uaccess"));
        assert!(UDEV_RULE.ends_with('\n'));
    }
}
