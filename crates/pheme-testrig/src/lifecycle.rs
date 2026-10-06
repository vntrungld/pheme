//! A VM's life during a run: revert, boot, prove the revert happened, and at
//! the end drop the canary and shut down. VM lifecycle stays in the shell
//! helpers under testrig/; this drives them.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vm::{self, RigError};

/// Only one run at a time, at the same path the shell helpers' `rig_lock`
/// uses, so the two exclude each other. Two overlapping runs would fight
/// over the same domains and produce a report mixing both, which is worse
/// than refusing.
#[derive(Debug)]
pub struct RunLock(PathBuf);

impl RunLock {
    pub fn acquire(state_dir: &Path) -> Result<RunLock, RigError> {
        std::fs::create_dir_all(state_dir)
            .map_err(|e| RigError(format!("creating {}: {e}", state_dir.display())))?;
        let lock = state_dir.join("lock");
        // create_dir, not create_dir_all: it fails when the directory exists,
        // which is the whole lock.
        std::fs::create_dir(&lock).map_err(|_| {
            RigError(format!(
                "another rig run holds {}; remove it if no run is active",
                lock.display()
            ))
        })?;
        let _ = std::fs::write(lock.join("pid"), std::process::id().to_string());
        Ok(RunLock(lock))
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Which operating system a domain runs, as the shell helpers name it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Windows,
}

impl Os {
    fn name(self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Windows => "windows",
        }
    }
}

/// Runs one of lib.sh's helpers with rig.conf loaded, from the repository
/// root, and returns its stdout.
fn helper(call: &str, args: &[&str]) -> Result<String, RigError> {
    vm::run(
        Command::new("sh")
            .arg("-c")
            .arg(format!(
                ". testrig/rig.conf && . testrig/lib.sh && {call} \"$@\""
            ))
            .arg("sh")
            .args(args),
    )
}

/// A variable from testrig/rig.conf, so domain names live in one place.
pub fn conf(var: &str) -> Result<String, RigError> {
    let value = vm::run(
        Command::new("sh")
            .arg("-c")
            .arg(format!(". testrig/rig.conf && printf '%s' \"${var}\"")),
    )?;
    if value.is_empty() {
        return Err(RigError(format!("testrig/rig.conf does not set {var}")));
    }
    Ok(value)
}

/// Reverts `domain` to its clean snapshot, boots it to a usable session,
/// and proves the revert happened. Returns the guest's address.
pub fn prepare(domain: &str, os: Os) -> Result<String, RigError> {
    vm::run(Command::new("sh").args(["testrig/snapshot.sh", "revert", domain]))?;
    let ip = helper("rig_boot", &[domain, os.name()])?.trim().to_string();
    if ip.is_empty() {
        return Err(RigError(format!("{domain} booted but reported no address")));
    }
    // The canary the previous run left must be gone. A revert that silently
    // did nothing would otherwise have every row judged on leftovers.
    helper("rig_canary_absent", &[&ip, "rig", os.name()]).map_err(|_| {
        RigError(format!(
            "{domain} still holds the previous run's canary: the snapshot revert did not happen"
        ))
    })?;
    Ok(ip)
}

/// Drops the canary the next run's revert must remove, then shuts the
/// domain down unless `keep`.
pub fn finish(domain: &str, os: Os, ip: &str, keep: bool) -> Result<(), RigError> {
    helper("rig_canary_write", &[ip, "rig", os.name()])?;
    if !keep {
        vm::virsh(&["shutdown", domain])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review Focus 5. The production change that breaks this: create the
    /// lock directory with `create_dir_all`, which succeeds when it already
    /// exists -- two runs would then share the VMs and interleave one report.
    #[test]
    fn a_second_run_is_refused_while_the_first_holds_the_lock() {
        let dir = tempfile::tempdir().expect("temp dir");
        let first = RunLock::acquire(dir.path()).expect("first run locks");
        let err = RunLock::acquire(dir.path()).expect_err("second run refused");
        assert!(err.0.contains("another rig run"), "{err}");
        drop(first);
        RunLock::acquire(dir.path()).expect("free again once the first run ends");
    }
}
