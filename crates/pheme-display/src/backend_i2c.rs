//! Linux: DDC/CI over `/dev/i2c-*`.
//!
//! `ddc_i2c::Enumerator` would find the buses, but it is behind the
//! `with-linux-enumerate` feature, which pulls `udev` and `libudev-sys`.
//! Reading the directory costs nothing and keeps a native C dependency out
//! of the build.

use std::path::{Path, PathBuf};

use ddc::{Ddc, Edid};
use ddc_i2c::I2cDeviceDdc;
use tracing::debug;

use crate::{edid::identity_from_edid, DisplayError, Monitor, INPUT_SELECT};

pub struct I2cMonitor {
    ddc: I2cDeviceDdc,
    identity: String,
    location: String,
}

impl Monitor for I2cMonitor {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn location(&self) -> &str {
        &self.location
    }

    fn get_input(&mut self) -> Result<u16, DisplayError> {
        self.ddc
            .get_vcp_feature(INPUT_SELECT)
            .map(|v| v.value())
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn set_input(&mut self, value: u16) -> Result<(), DisplayError> {
        self.ddc
            .set_vcp_feature(INPUT_SELECT, value)
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn capabilities(&mut self) -> Result<String, DisplayError> {
        let raw = self
            .ddc
            .capabilities_string()
            .map_err(|e| DisplayError::Backend(e.to_string()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

pub fn enumerate() -> Vec<Box<dyn Monitor>> {
    // `i2c_paths` returns them ordered by bus number, so the "first
    // monitor" a person gets with no `display.monitor` is the same one on
    // every run. `read_dir` order is not.
    i2c_paths().iter().filter_map(|p| open_bus(p)).collect()
}

fn i2c_paths() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(n) = name.strip_prefix("i2c-") else {
            continue;
        };
        if n.parse::<u32>().is_ok() {
            out.push(e.path());
        }
    }
    // `sort` on the paths would order i2c-10 before i2c-2, so sort on the
    // number the name carries.
    out.sort_by_key(|p| bus_number(p).unwrap_or(u32::MAX));
    out
}

fn bus_number(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("i2c-")?
        .parse()
        .ok()
}

/// One bus, kept only when something on it answers a read of VCP 0x60.
fn open_bus(path: &Path) -> Option<Box<dyn Monitor>> {
    let mut ddc = match ddc_i2c::from_i2c_device(path) {
        Ok(d) => d,
        Err(e) => {
            debug!(path = %path.display(), error = %e, "opening the i2c device failed");
            return None;
        }
    };
    let location = path.display().to_string();
    let mut raw = vec![0u8; 128];
    let identity = match ddc.read_edid(0, &mut raw) {
        Ok(_) => identity_from_edid(&raw),
        Err(_) => None,
    }
    .unwrap_or_else(|| location.clone());
    if let Err(e) = ddc.get_vcp_feature(INPUT_SELECT) {
        debug!(monitor = %identity, error = %e, "does not answer VCP 0x60; skipping");
        return None;
    }
    Some(Box::new(I2cMonitor {
        ddc,
        identity,
        location,
    }))
}
