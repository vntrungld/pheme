//! Windows: DDC/CI through Dxva2's `SetVCPFeature`.

use ddc::Ddc;
use ddc_winapi::Monitor as WinMonitor;
use tracing::{debug, warn};

use crate::{DisplayError, Monitor, INPUT_SELECT};

pub struct WinApiMonitor {
    ddc: WinMonitor,
    /// `description()` is all the Win32 API offers, so identity and
    /// location are the same string here.
    description: String,
}

impl Monitor for WinApiMonitor {
    fn identity(&self) -> &str {
        &self.description
    }

    fn location(&self) -> &str {
        &self.description
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
    let monitors = match WinMonitor::enumerate() {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "enumerating physical monitors failed");
            return Vec::new();
        }
    };
    monitors
        .into_iter()
        .filter_map(|mut m| {
            let description = m.description();
            if let Err(e) = m.get_vcp_feature(INPUT_SELECT) {
                debug!(monitor = %description, error = %e, "does not answer VCP 0x60; skipping");
                return None;
            }
            Some(Box::new(WinApiMonitor {
                ddc: m,
                description,
            }) as Box<dyn Monitor>)
        })
        .collect()
}
