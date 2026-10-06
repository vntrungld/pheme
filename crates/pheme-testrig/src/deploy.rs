//! Installing the real packages in the VMs.
//!
//! The rig installs the `.deb` and the `.exe` installer rather than copying
//! loose binaries, so what is tested is what a user receives -- and rows F1,
//! F3 and F6 fall out of this step instead of needing their own installs.

use std::path::{Path, PathBuf};
use std::process::Command;

use crate::vm::{self, Remote, RigError};

/// Where the packages came from. Printed in the report: a pass against a
/// locally built binary proves less than one against the shipped artifact,
/// and the reader is entitled to know which they are looking at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    /// A GitHub Actions run of `release.yml`, by id.
    CiRun(String),
    /// A local directory holding both packages.
    Local(PathBuf),
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::CiRun(id) => write!(f, "CI run {id}"),
            Source::Local(dir) => write!(f, "local build in {}", dir.display()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Artifacts {
    pub deb: PathBuf,
    pub setup_exe: PathBuf,
}

impl Artifacts {
    /// Finds exactly one `*_amd64.deb` and one `*-setup.exe` in `dir`,
    /// matched by shape so a version bump is not mistaken for a missing
    /// package. Zero or several of either is a rig error. The `.rpm` is not
    /// looked for: the rig has no Fedora VM, and row F2 is skipped.
    pub fn from_dir(dir: &Path) -> Result<Artifacts, RigError> {
        let names: Vec<PathBuf> = std::fs::read_dir(dir)
            .map_err(|e| RigError(format!("reading {}: {e}", dir.display())))?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .collect();
        let one = |suffix: &str, what: &str| -> Result<PathBuf, RigError> {
            let found: Vec<&PathBuf> = names
                .iter()
                .filter(|p| {
                    p.file_name()
                        .and_then(|n| n.to_str())
                        .is_some_and(|n| n.starts_with("pheme") && n.ends_with(suffix))
                })
                .collect();
            match found.as_slice() {
                [only] => Ok((*only).clone()),
                [] => Err(RigError(format!("no {what} in {}", dir.display()))),
                many => Err(RigError(format!(
                    "{} candidates for the {what} in {}; leave exactly one",
                    many.len(),
                    dir.display()
                ))),
            }
        };
        Ok(Artifacts {
            deb: one("_amd64.deb", ".deb")?,
            setup_exe: one("-setup.exe", "Windows installer")?,
        })
    }
}

/// Gets the packages: downloads a CI run's two artifacts into `into`, or
/// takes a local directory as it is.
pub fn fetch(source: &Source, into: &Path) -> Result<Artifacts, RigError> {
    match source {
        Source::Local(dir) => Artifacts::from_dir(dir),
        Source::CiRun(id) => {
            for name in ["pheme-x86_64-linux", "pheme-x86_64-windows"] {
                vm::run(
                    Command::new("gh")
                        .args(["run", "download", id, "--name", name, "--dir"])
                        .arg(into),
                )?;
            }
            Artifacts::from_dir(into)
        }
    }
}

fn file_name(p: &Path) -> Result<String, RigError> {
    p.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| RigError(format!("{} has no file name", p.display())))
}

/// Installs the `.deb` with apt, so its dependencies resolve as they would
/// for a user. The result is the product's answer: a failed install is a
/// finding for row F1, not a rig error.
pub fn install_ubuntu(ip: &str, a: &Artifacts) -> Result<Remote, RigError> {
    let name = file_name(&a.deb)?;
    vm::scp(&a.deb, ip, "rig", &format!("/tmp/{name}"))?;
    vm::ssh(
        ip,
        "rig",
        &format!("sudo DEBIAN_FRONTEND=noninteractive apt-get install -y /tmp/{name}"),
    )
}

/// Runs the installer silently as `rig`, a standard user: with
/// `PrivilegesRequired=lowest` it must need no elevation, and over ssh an
/// elevation request fails rather than prompting.
pub fn install_windows(ip: &str, a: &Artifacts) -> Result<Remote, RigError> {
    let name = file_name(&a.setup_exe)?;
    vm::scp(&a.setup_exe, ip, "rig", &name)?;
    vm::ssh(
        ip,
        "rig",
        &format!(
            "$p = Start-Process -Wait -PassThru -FilePath \"$HOME\\{name}\" \
             -ArgumentList '/VERYSILENT','/SUPPRESSMSGBOXES','/TASKS=startup,addtopath'; \
             exit $p.ExitCode"
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// cargo-deb emits `pheme_0.1.0-1_amd64.deb` -- with a Debian revision --
    /// and the installer carries a version too. The production change that
    /// breaks this: match on an exact filename. A version bump would then make
    /// deploy silently find nothing, and every row would report a rig error on
    /// a perfectly good build.
    #[test]
    fn the_deb_is_found_by_shape_not_by_exact_name() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("pheme_9.9.9-1_amd64.deb"), b"x").expect("write");
        fs::write(
            dir.path().join("pheme-9.9.9-x86_64-windows-setup.exe"),
            b"x",
        )
        .expect("write");
        let a = Artifacts::from_dir(dir.path()).expect("both artifacts found");
        assert!(a
            .deb
            .file_name()
            .expect("name")
            .to_string_lossy()
            .ends_with("_amd64.deb"));
        assert!(a
            .setup_exe
            .file_name()
            .expect("name")
            .to_string_lossy()
            .ends_with("-setup.exe"));
    }

    /// Two builds in one directory is ambiguous, and picking one at random
    /// would mean the report names a version that is not what was tested. The
    /// production change that breaks this: take the first match.
    #[test]
    fn two_debs_in_one_directory_is_an_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("pheme_1.0.0-1_amd64.deb"), b"x").expect("write");
        fs::write(dir.path().join("pheme_2.0.0-1_amd64.deb"), b"x").expect("write");
        fs::write(
            dir.path().join("pheme-1.0.0-x86_64-windows-setup.exe"),
            b"x",
        )
        .expect("write");
        assert!(Artifacts::from_dir(dir.path()).is_err());
    }

    /// A missing artifact is a rig error, not a product failure.
    #[test]
    fn a_missing_artifact_is_reported_as_a_rig_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        let err = Artifacts::from_dir(dir.path()).expect_err("should fail");
        assert!(err.0.contains("no .deb"), "message was {err}");
    }

    /// CI uploads `pheme_*.deb*`, so a checksum can sit beside the package.
    /// The production change that breaks this: match on "contains .deb",
    /// which counts `pheme_..._amd64.deb.sha256` as a second package and
    /// refuses a perfectly good download as ambiguous.
    #[test]
    fn a_checksum_beside_the_deb_is_not_a_second_deb() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(dir.path().join("pheme_1.0.0-1_amd64.deb"), b"x").expect("write");
        fs::write(dir.path().join("pheme_1.0.0-1_amd64.deb.sha256"), b"x").expect("write");
        fs::write(
            dir.path().join("pheme-1.0.0-x86_64-windows-setup.exe"),
            b"x",
        )
        .expect("write");
        fs::write(
            dir.path()
                .join("pheme-1.0.0-x86_64-windows-setup.exe.sha256"),
            b"x",
        )
        .expect("write");
        Artifacts::from_dir(dir.path()).expect("one of each");
    }
}
