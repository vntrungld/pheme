//! Rows F1, F3 and F6, which fall out of installing the real `.deb`.
//!
//! The judges are pure and tested; the commands only gather their evidence.

use crate::deploy::{self, Artifacts};
use crate::verdict::{RowResult, Verdict};
use crate::vm::{self, Remote, RigError};

/// F1: apt installs the `.deb`, and every library pheme opens with `dlopen`
/// resolves. `pheme --version` alone exits inside clap before the first
/// dlopen, which is why packaging/check-dlopen-sonames.sh exists.
///
/// Mutation that breaks it: drop a library from the hand-written `depends`
/// in crates/pheme-app/Cargo.toml. On this rig's VM that mutation is only
/// visible for libraries the desktop does not already carry -- the VM has
/// GNOME, so "a machine that never had GTK" is packaging/verify-deb.sh's to
/// prove, in a clean container.
pub fn judge_f1(install: &Remote, sonames: &Remote) -> (Verdict, String) {
    if !install.ok() {
        return (
            Verdict::Fail,
            format!(
                "apt-get install exited {}: {}",
                install.code,
                last_line(install)
            ),
        );
    }
    let missing: Vec<&str> = sonames
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("MISSING:").map(str::trim))
        .collect();
    if !missing.is_empty() || !sonames.ok() {
        return (
            Verdict::Fail,
            format!("dlopen'd sonames do not resolve: {}", missing.join(", ")),
        );
    }
    (
        Verdict::Pass,
        "installed; every dlopen'd soname resolves (the VM has a desktop, so the no-GTK half is verify-deb.sh's)".into(),
    )
}

/// F3: right after install, the i2c nodes are open to the logged-in user
/// and `pheme displays` runs without a permission error.
///
/// Mutation that breaks it: remove the i2c line from the packaged udev rule;
/// the stub adapter's node stays root-only.
pub fn judge_f3(access: &Remote, displays: &Remote) -> (Verdict, String) {
    let nodes: Vec<&str> = access.stdout.lines().filter(|l| !l.is_empty()).collect();
    if nodes.is_empty() {
        return (
            Verdict::Inconclusive,
            "no /dev/i2c-* node to check; i2c-stub did not load".into(),
        );
    }
    let denied: Vec<&str> = nodes
        .iter()
        .filter(|l| l.ends_with(" denied"))
        .map(|l| l.trim_end_matches(" denied"))
        .collect();
    if !denied.is_empty() {
        return (
            Verdict::Fail,
            format!("not accessible to the user: {}", denied.join(", ")),
        );
    }
    let said = format!("{}{}", displays.stdout, displays.stderr).to_lowercase();
    if !displays.ok() || said.contains("permission") {
        return (
            Verdict::Fail,
            format!(
                "pheme displays exited {}: {}",
                displays.code,
                last_line(displays)
            ),
        );
    }
    (
        Verdict::Pass,
        format!(
            "{} i2c node(s) accessible; pheme displays ran clean",
            nodes.len()
        ),
    )
}

/// F6: remove then purge leave no binary, unit, udev rule, module list or
/// desktop entry, and leave the user's config.toml exactly as it was.
///
/// Mutation that breaks it: add config.toml to the package's files; purge
/// then deletes the user's configuration.
pub fn judge_f6(after: &Remote) -> (Verdict, String) {
    let left: Vec<&str> = after
        .stdout
        .lines()
        .filter_map(|l| l.strip_prefix("left: "))
        .collect();
    let config = after
        .stdout
        .lines()
        .find_map(|l| l.strip_prefix("config: "));
    match (left.is_empty(), config) {
        (_, None) => (
            Verdict::Inconclusive,
            format!(
                "the check stopped before reporting the config: {}",
                last_line(after)
            ),
        ),
        (false, _) => (Verdict::Fail, format!("left behind: {}", left.join(", "))),
        (true, Some("intact")) => (
            Verdict::Pass,
            "nothing left behind; the user's config.toml untouched".into(),
        ),
        (true, Some(other)) => (Verdict::Fail, format!("the user's config.toml is {other}")),
    }
}

fn last_line(r: &Remote) -> String {
    let both = format!("{}\n{}", r.stdout, r.stderr);
    both.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

const F3_ACCESS: &str = "\
sudo modprobe i2c-dev && sudo modprobe i2c-stub chip_addr=0x37 && sudo udevadm settle
for d in /dev/i2c-*; do
  [ -e \"$d\" ] || continue
  if [ -r \"$d\" ] && [ -w \"$d\" ]; then echo \"$d rw\"; else echo \"$d denied\"; fi
done";

const F6_PURGE: &str = "\
mkdir -p ~/.config/pheme && printf '# pheme-rig marker\\n' > ~/.config/pheme/config.toml
sudo DEBIAN_FRONTEND=noninteractive apt-get remove -y pheme >/dev/null
sudo DEBIAN_FRONTEND=noninteractive apt-get purge -y pheme >/dev/null
for p in /usr/bin/pheme /usr/lib/udev/rules.d/80-pheme.rules \
         /usr/lib/modules-load.d/pheme.conf /usr/lib/systemd/user/pheme.service \
         /usr/share/applications/pheme.desktop; do
  [ -e \"$p\" ] && echo \"left: $p\"
done
if [ \"$(cat ~/.config/pheme/config.toml 2>/dev/null)\" = '# pheme-rig marker' ]; then
  echo 'config: intact'
elif [ -e ~/.config/pheme/config.toml ]; then echo 'config: changed'
else echo 'config: deleted'; fi";

fn result(id: &str, what: &str, (verdict, evidence): (Verdict, String)) -> RowResult {
    RowResult {
        id: id.into(),
        what: what.into(),
        verdict,
        evidence,
    }
}

/// Installs the `.deb` on the Ubuntu VM at `ip` and judges whichever of F1,
/// F3 and F6 are in `wanted`. F6 removes the package, so it runs last.
pub fn run(ip: &str, a: &Artifacts, wanted: &[&str]) -> Result<Vec<RowResult>, RigError> {
    let install = deploy::install_ubuntu(ip, a)?;
    let mut out = Vec::new();
    if wanted.contains(&"F1") {
        vm::scp(
            std::path::Path::new("packaging/check-dlopen-sonames.sh"),
            ip,
            "rig",
            "/tmp/check-dlopen-sonames.sh",
        )?;
        let sonames = vm::ssh(ip, "rig", "sh /tmp/check-dlopen-sonames.sh")?;
        out.push(result(
            "F1",
            "apt install of the .deb on a machine with no GTK",
            judge_f1(&install, &sonames),
        ));
    }
    if !install.ok() {
        // Nothing to judge without the package; that is F1's finding, not
        // F3's or F6's.
        for id in wanted.iter().filter(|id| **id != "F1") {
            out.push(result(
                id,
                "",
                (
                    Verdict::Inconclusive,
                    "the .deb did not install; see F1".into(),
                ),
            ));
        }
        return Ok(out);
    }
    if wanted.contains(&"F3") {
        let access = vm::ssh(ip, "rig", F3_ACCESS)?;
        let displays = vm::ssh(ip, "rig", "pheme displays")?;
        out.push(result(
            "F3",
            "pheme displays right after install",
            judge_f3(&access, &displays),
        ));
    }
    if wanted.contains(&"F6") {
        let after = vm::ssh(ip, "rig", F6_PURGE)?;
        out.push(result(
            "F6",
            "remove and purge keep only the user config",
            judge_f6(&after),
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(code: i32, stdout: &str) -> Remote {
        Remote {
            code,
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    const ALL_FOUND: &str = "found:   libGL.so.1\nall dlopen'd sonames resolve\n";

    #[test]
    fn f1_passes_when_install_and_sonames_both_succeed() {
        let (v, _) = judge_f1(&remote(0, ""), &remote(0, ALL_FOUND));
        assert_eq!(v, Verdict::Pass);
    }

    /// The production change that breaks this: judge F1 on the install's
    /// exit code alone. `pheme --version` and apt both succeed with a
    /// dlopen'd library missing, and the user gets a pheme that cannot open
    /// a window.
    #[test]
    fn f1_fails_on_a_missing_dlopen_soname_even_when_apt_succeeded() {
        let out = "found:   libGL.so.1\nMISSING: libEGL.so.1\n";
        let (v, evidence) = judge_f1(&remote(0, ""), &remote(1, out));
        assert_eq!(v, Verdict::Fail);
        assert!(evidence.contains("libEGL.so.1"), "{evidence}");
    }

    #[test]
    fn f1_fails_when_apt_fails() {
        let (v, _) = judge_f1(&remote(100, "E: Unmet dependencies"), &remote(0, ALL_FOUND));
        assert_eq!(v, Verdict::Fail);
    }

    /// The production change that breaks this: drop the i2c line from the
    /// packaged udev rule. The stub adapter's node then stays root-only and
    /// rig cannot open it.
    #[test]
    fn f3_fails_when_an_i2c_node_is_not_accessible() {
        let access = "/dev/i2c-0 rw\n/dev/i2c-1 denied\n";
        let (v, evidence) = judge_f3(&remote(0, access), &remote(0, ""));
        assert_eq!(v, Verdict::Fail);
        assert!(evidence.contains("/dev/i2c-1"), "{evidence}");
    }

    /// With no i2c node at all, nothing about the rule was observed. The
    /// production change this guards against is in the rig: a row that
    /// passes because there was nothing to check could never fail.
    #[test]
    fn f3_with_no_i2c_node_is_inconclusive_not_pass() {
        let (v, _) = judge_f3(&remote(0, ""), &remote(0, ""));
        assert_eq!(v, Verdict::Inconclusive);
    }

    #[test]
    fn f3_fails_on_a_permission_error_from_pheme_displays() {
        let displays = Remote {
            code: 1,
            stdout: String::new(),
            stderr: "Error: permission denied: /dev/i2c-0".into(),
        };
        let (v, _) = judge_f3(&remote(0, "/dev/i2c-0 rw\n"), &displays);
        assert_eq!(v, Verdict::Fail);
    }

    #[test]
    fn f3_passes_with_access_and_a_clean_listing() {
        let (v, _) = judge_f3(&remote(0, "/dev/i2c-0 rw\n"), &remote(0, ""));
        assert_eq!(v, Verdict::Pass);
    }

    /// The production change that breaks this: add config.toml to the
    /// package's files. purge then deletes the user's configuration.
    #[test]
    fn f6_fails_when_purge_touched_the_user_config() {
        let (v, _) = judge_f6(&remote(0, "config: changed\n"));
        assert_eq!(v, Verdict::Fail);
    }

    #[test]
    fn f6_fails_on_anything_left_behind() {
        let (v, evidence) = judge_f6(&remote(
            0,
            "left: /usr/lib/udev/rules.d/80-pheme.rules\nconfig: intact\n",
        ));
        assert_eq!(v, Verdict::Fail);
        assert!(evidence.contains("80-pheme.rules"), "{evidence}");
    }

    /// The script reports the config last; output cut short before that
    /// line proves nothing about it.
    #[test]
    fn f6_without_a_config_line_is_inconclusive() {
        let (v, _) = judge_f6(&remote(0, ""));
        assert_eq!(v, Verdict::Inconclusive);
    }

    #[test]
    fn f6_passes_when_nothing_is_left_and_the_config_is_intact() {
        let (v, _) = judge_f6(&remote(0, "config: intact\n"));
        assert_eq!(v, Verdict::Pass);
    }
}
