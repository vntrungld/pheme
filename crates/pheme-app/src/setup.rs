//! `pheme setup`: one-time OS prerequisites.

use anyhow::Context;

pub const UDEV_RULE: &str = "# Pheme: allow members of the input group to create virtual input devices\nKERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"\n";

#[cfg(target_os = "linux")]
pub fn run() -> anyhow::Result<()> {
    use std::path::Path;
    use std::process::Command;

    let rule_path = Path::new("/etc/udev/rules.d/80-pheme.rules");
    let user = std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_default();
    let is_root = unsafe { libc_geteuid() } == 0;
    if !is_root {
        println!("Run the following as root (or re-run `sudo pheme setup`):");
        println!();
        println!("  cat > {} <<'EOF'\n{}EOF", rule_path.display(), UDEV_RULE);
        println!("  udevadm control --reload && udevadm trigger --name-match=uinput");
        println!("  usermod -aG input {user}");
        println!();
        println!("Then log out and back in so the group change applies.");
        return Ok(());
    }
    std::fs::write(rule_path, UDEV_RULE)
        .with_context(|| format!("writing {}", rule_path.display()))?;
    println!("Wrote {}", rule_path.display());
    let _ = Command::new("udevadm")
        .args(["control", "--reload"])
        .status();
    let _ = Command::new("udevadm")
        .args(["trigger", "--name-match=uinput"])
        .status();
    let _ = Command::new("modprobe").arg("uinput").status();
    if !user.is_empty() && user != "root" {
        let st = Command::new("usermod")
            .args(["-aG", "input", &user])
            .status()
            .context("running usermod")?;
        if st.success() {
            println!("Added {user} to the input group. Log out and back in for it to apply.");
        }
    }
    println!("Setup complete.");
    Ok(())
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
    fn udev_rule_targets_uinput_for_the_input_group() {
        assert!(UDEV_RULE.contains("KERNEL==\"uinput\""));
        assert!(UDEV_RULE.contains("GROUP=\"input\""));
        assert!(UDEV_RULE.contains("uaccess"));
        assert!(UDEV_RULE.ends_with('\n'));
    }
}
