//! The runner's hands on the VMs: `virsh` and `ssh`, with every failure
//! typed as a [`RigError`] so that no caller can turn an infrastructure
//! problem into a product FAIL by accident.

use std::process::Command;

use crate::verdict::Verdict;

/// Something about the rig, not about pheme, went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RigError(pub String);

impl std::fmt::Display for RigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "rig error: {}", self.0)
    }
}

impl std::error::Error for RigError {}

impl RigError {
    /// Always INCONCLUSIVE: see [`Verdict::from_rig_error`].
    pub fn verdict(&self) -> Verdict {
        Verdict::from_rig_error(&self.0)
    }
}

/// Runs a command and returns its stdout, or a [`RigError`] naming it.
pub fn run(cmd: &mut Command) -> Result<String, RigError> {
    let shown = format!("{cmd:?}");
    let out = cmd
        .output()
        .map_err(|e| RigError(format!("could not run {shown}: {e}")))?;
    if !out.status.success() {
        return Err(RigError(format!(
            "{shown} exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

pub fn virsh(args: &[&str]) -> Result<String, RigError> {
    run(Command::new("virsh")
        .arg("-c")
        .arg("qemu:///system")
        .args(args))
}

/// The guest's IPv4 address from `virsh domifaddr` output, if it has a lease.
pub fn parse_domifaddr(out: &str) -> Option<String> {
    out.lines().find_map(|l| {
        let cols: Vec<&str> = l.split_whitespace().collect();
        match cols.as_slice() {
            [_, _, "ipv4", addr, ..] => addr.split('/').next().map(str::to_string),
            _ => None,
        }
    })
}

pub fn vm_ip(domain: &str) -> Result<Option<String>, RigError> {
    Ok(parse_domifaddr(&virsh(&["domifaddr", domain])?))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The production change that breaks this: take the first address column
    /// regardless of protocol, which returns an IPv6 link-local address the
    /// rig's ssh cannot use, and every row goes INCONCLUSIVE.
    #[test]
    fn the_ipv4_lease_is_read_from_domifaddr() {
        let out = " Name       MAC address          Protocol     Address\n\
                   -------------------------------------------------------\n \
                   vnet3      52:54:00:12:34:56    ipv6         fe80::1/64\n \
                   vnet3      52:54:00:12:34:56    ipv4         192.168.140.57/24\n";
        assert_eq!(parse_domifaddr(out), Some("192.168.140.57".to_string()));
    }

    #[test]
    fn no_lease_yet_is_none() {
        assert_eq!(
            parse_domifaddr(" Name  MAC address  Protocol  Address\n----\n"),
            None
        );
    }
}
