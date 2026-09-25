//! Listing the audio devices the operating system offers.
//!
//! Two platform backends with no crate between them and the system, because
//! the configuration window's device menus are unusable without it and
//! `pheme devices` has been in the architecture document since the beginning
//! without ever existing. Sub-project 6 design §7.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeviceKind {
    Playback,
    Capture,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub name: String,
    pub kind: DeviceKind,
    pub is_default: bool,
}

/// Playback first, then capture, each alphabetical.
///
/// A stable order matters more than the particular order: a menu that
/// reshuffles between openings is a menu people mis-click.
fn sort_devices(v: &mut [DeviceInfo]) {
    v.sort_by(|a, b| match (a.kind, b.kind) {
        (DeviceKind::Playback, DeviceKind::Capture) => std::cmp::Ordering::Less,
        (DeviceKind::Capture, DeviceKind::Playback) => std::cmp::Ordering::Greater,
        _ => a.name.cmp(&b.name),
    });
}

/// Not `cfg(target_os = "linux")`, although only the Linux backend calls it:
/// its test runs on every platform, and a `cfg` here would break the Windows
/// build of the test module. It is pure string matching and costs nothing
/// where it is unused.
fn kind_from_media_class(class: &str) -> Option<DeviceKind> {
    match class {
        "Audio/Sink" => Some(DeviceKind::Playback),
        "Audio/Source" => Some(DeviceKind::Capture),
        // Stream/* are applications playing or recording, not devices, and
        // listing them would offer the user something they cannot select.
        _ => None,
    }
}

/// Lists the audio devices PipeWire currently offers.
#[cfg(target_os = "linux")]
pub fn list_devices() -> crate::Result<Vec<DeviceInfo>> {
    linux::list_devices()
}

/// Lists the audio devices WASAPI currently offers.
#[cfg(target_os = "windows")]
pub fn list_devices() -> crate::Result<Vec<DeviceInfo>> {
    windows_backend::list_devices()
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn list_devices() -> crate::Result<Vec<DeviceInfo>> {
    Err(crate::Error::Unsupported(
        "no audio device enumeration backend for this platform".into(),
    ))
}

/// The Linux backend: a short-lived PipeWire main loop that walks the
/// registry once and quits.
///
/// Follows the connection and main-loop patterns already established in
/// `linux_pipewire.rs` (a `MainLoopRc`/`ContextRc`/`CoreRc` built as locals in
/// one function) rather than inventing a second way of talking to PipeWire in
/// this crate.
#[cfg(target_os = "linux")]
mod linux {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::mpsc;
    use std::time::Duration;

    use pipewire as pw;
    use tracing::warn;

    use super::{kind_from_media_class, sort_devices, DeviceInfo, DeviceKind};
    use crate::{Error, Result};

    /// How long `list_devices` waits for PipeWire to answer before giving up. Matches
    /// `linux_pipewire.rs`'s `START_TIMEOUT`, both in value and in what it guards against: a
    /// daemon that is running but never replies must not hang the caller forever. Unlike that
    /// file's device threads, there is nothing useful to keep running past this deadline — a
    /// one-shot enumeration that has not answered is just wrong, not degraded — so on timeout
    /// this reports an error rather than the empty list a caller could mistake for "no
    /// devices."
    const ENUMERATE_TIMEOUT: Duration = Duration::from_secs(1);

    /// A node global as reported by the registry, before it is known whether
    /// it is the default. `node_name` (not the display `name`, which prefers
    /// `node.description`) is what `default.audio.sink`/`default.audio.source`
    /// metadata refers to.
    struct RawNode {
        name: String,
        kind: DeviceKind,
        node_name: String,
    }

    /// Runs the enumeration on its own thread and bounds the wait for it, the same shape as
    /// `crate::device::DeviceThread::start`: a PipeWire loop that has not yet reached the point
    /// where it can observe a stop request may never act on one, so joining it would turn this
    /// bounded wait into an unbounded one. On timeout the thread is abandoned, detached, never
    /// joined, exactly as that type documents doing.
    pub(super) fn list_devices() -> Result<Vec<DeviceInfo>> {
        let (tx, rx) = mpsc::channel::<Result<Vec<DeviceInfo>>>();
        let thread = std::thread::Builder::new()
            .name("pheme-pw-devices".into())
            .spawn(move || {
                let _ = tx.send(enumerate());
            })
            .map_err(|e| {
                Error::Device(format!("spawning the PipeWire device-listing thread: {e}"))
            })?;

        match rx.recv_timeout(ENUMERATE_TIMEOUT) {
            Ok(result) => {
                let _ = thread.join();
                result
            }
            Err(_) => {
                warn!(
                    "PipeWire did not answer within {ENUMERATE_TIMEOUT:?}; abandoning the \
                     device-listing thread detached rather than blocking `list_devices` further"
                );
                drop(thread);
                Err(Error::Device(format!(
                    "PipeWire did not answer within {ENUMERATE_TIMEOUT:?}"
                )))
            }
        }
    }

    /// The actual PipeWire work, run on the thread `list_devices` spawns and bounds.
    fn enumerate() -> Result<Vec<DeviceInfo>> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopRc::new(None)
            .map_err(|e| Error::Device(format!("creating the PipeWire main loop: {e}")))?;
        let context = pw::context::ContextRc::new(&mainloop, None)
            .map_err(|e| Error::Device(format!("creating the PipeWire context: {e}")))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| Error::Device(format!("connecting to PipeWire: {e}")))?;
        let registry = core
            .get_registry_rc()
            .map_err(|e| Error::Device(format!("getting the PipeWire registry: {e}")))?;

        let nodes: Rc<RefCell<Vec<RawNode>>> = Rc::new(RefCell::new(Vec::new()));
        let default_sink: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        let default_source: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        // Keeps the "default" metadata proxy and its listener alive for the life of the main
        // loop. A registry `global` callback is the only place we ever see that global, so this
        // is where it gets filled in.
        let metadata: Rc<
            RefCell<Option<(pw::metadata::Metadata, pw::metadata::MetadataListener)>>,
        > = Rc::new(RefCell::new(None));

        let registry_for_global = registry.clone();
        let nodes_for_global = nodes.clone();
        let default_sink_for_global = default_sink.clone();
        let default_source_for_global = default_source.clone();
        let metadata_for_global = metadata.clone();
        let _registry_listener = registry
            .add_listener_local()
            .global(move |obj| {
                use pw::types::ObjectType;
                match obj.type_ {
                    ObjectType::Node => {
                        let Some(props) = obj.props else {
                            return;
                        };
                        let Some(kind) = props.get("media.class").and_then(kind_from_media_class)
                        else {
                            return;
                        };
                        let node_name = props.get("node.name").unwrap_or("").to_string();
                        let name = props
                            .get("node.description")
                            .filter(|s| !s.is_empty())
                            .or_else(|| props.get("node.name"))
                            .unwrap_or("")
                            .to_string();
                        if name.is_empty() {
                            return;
                        }
                        nodes_for_global.borrow_mut().push(RawNode {
                            name,
                            kind,
                            node_name,
                        });
                    }
                    ObjectType::Metadata => {
                        let Some(props) = obj.props else {
                            return;
                        };
                        // There can be more than one metadata object (e.g. "route-settings");
                        // only "default" carries the default sink/source.
                        if props.get("metadata.name") != Some("default") {
                            return;
                        }
                        let md: pw::metadata::Metadata = match registry_for_global.bind(obj) {
                            Ok(md) => md,
                            Err(_) => return,
                        };
                        let sink = default_sink_for_global.clone();
                        let source = default_source_for_global.clone();
                        let listener = md
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                match (key, value) {
                                    (Some("default.audio.sink"), Some(v)) => {
                                        *sink.borrow_mut() = extract_name(v);
                                    }
                                    (Some("default.audio.source"), Some(v)) => {
                                        *source.borrow_mut() = extract_name(v);
                                    }
                                    _ => {}
                                }
                                0
                            })
                            .register();
                        *metadata_for_global.borrow_mut() = Some((md, listener));
                    }
                    _ => {}
                }
            })
            .register();

        // Two round trips, not one. The first (`sync(0)`) guarantees every global that existed
        // when we connected — including the "default" metadata object — has been delivered and,
        // for metadata, bound. Binding metadata sends its own request, whose reply (the initial
        // dump of every property, including both defaults) can only arrive *after* that first
        // round trip's `done`, since it was sent later. The second round trip (`sync(1)`, issued
        // from inside the first `done`) is what waits for that reply before the loop is allowed
        // to quit.
        let phase: Rc<Cell<u8>> = Rc::new(Cell::new(0));
        let pending: Rc<Cell<pw::spa::utils::result::AsyncSeq>> =
            Rc::new(Cell::new(core.sync(0).map_err(|e| {
                Error::Device(format!("starting the PipeWire sync: {e}"))
            })?));

        // Set when the core reports a fatal, non-recoverable error (the daemon going away, the
        // connection breaking) — the same signal `linux_pipewire.rs`'s backends end their main
        // loop on. Without this, a connection that fails *after* `connect_rc` already succeeded
        // would either hang (nothing ever quits the loop) or, worse, quit some other way and
        // silently report whatever partial list had been collected as if it were complete.
        let core_error: Rc<Cell<bool>> = Rc::new(Cell::new(false));

        let quit_loop = mainloop.clone();
        let core_for_done = core.clone();
        let phase_for_done = phase.clone();
        let pending_for_done = pending.clone();
        let core_error_for_listener = core_error.clone();
        let quit_loop_for_error = mainloop.clone();
        let _core_listener = core
            .add_listener_local()
            .done(move |id, seq| {
                if id != pw::core::PW_ID_CORE || seq != pending_for_done.get() {
                    return;
                }
                if phase_for_done.get() == 0 {
                    phase_for_done.set(1);
                    match core_for_done.sync(1) {
                        Ok(next) => pending_for_done.set(next),
                        // Nothing more to wait for; report what was collected so far.
                        Err(_) => quit_loop.quit(),
                    }
                } else {
                    quit_loop.quit();
                }
            })
            .error(move |id, seq, res, message| {
                warn!(
                    id,
                    seq, res, message, "the PipeWire connection failed while listing devices"
                );
                core_error_for_listener.set(true);
                quit_loop_for_error.quit();
            })
            .register();

        mainloop.run();

        if core_error.get() {
            return Err(Error::Device(
                "the PipeWire connection failed while listing devices".into(),
            ));
        }

        let raw = nodes.borrow();
        let default_sink = default_sink.borrow();
        let default_source = default_source.borrow();
        let mut devices: Vec<DeviceInfo> = raw
            .iter()
            .map(|n| {
                let is_default = match n.kind {
                    DeviceKind::Playback => default_sink.as_deref() == Some(n.node_name.as_str()),
                    DeviceKind::Capture => default_source.as_deref() == Some(n.node_name.as_str()),
                };
                DeviceInfo {
                    name: n.name.clone(),
                    kind: n.kind,
                    is_default,
                }
            })
            .collect();
        sort_devices(&mut devices);
        Ok(devices)
    }

    /// Pulls the `"name"` field out of a `default.audio.sink`/`default.audio.source` metadata
    /// value, which PipeWire always sends as a small flat JSON object, e.g.
    /// `{"name":"alsa_output.pci-0000_00_1f.3.analog-stereo"}`. A hand-rolled scan rather than a
    /// JSON crate for one field: PipeWire's own clients build and consume this string without
    /// nesting or escaping it.
    fn extract_name(json: &str) -> Option<String> {
        let after_key = json.split_once("\"name\"")?.1;
        let after_colon = after_key.split_once(':')?.1.trim_start();
        let quoted = after_colon.strip_prefix('"')?;
        let end = quoted.find('"')?;
        Some(quoted[..end].to_string())
    }
}

/// The Windows backend: active render and capture endpoints via `IMMDeviceEnumerator`,
/// with the console default for each flow marked `is_default`.
///
/// Reuses `ComGuard` and `friendly_name` from `windows::wasapi` rather than inventing a
/// second way of initialising COM or reading a property store in this crate.
#[cfg(target_os = "windows")]
mod windows_backend {
    use windows::Win32::Media::Audio::{
        eCapture, eConsole, eRender, EDataFlow, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
        DEVICE_STATE_ACTIVE,
    };
    use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};

    use super::{sort_devices, DeviceInfo, DeviceKind};
    use crate::windows::wasapi::{friendly_name, ComGuard};
    use crate::{Error, Result};

    pub(super) fn list_devices() -> Result<Vec<DeviceInfo>> {
        // SAFETY: a standard MMDevice enumeration, the same shape `open_render_device` and
        // `open_capture_device` in `wasapi.rs` already use. Every raw pointer stays inside
        // this module, and the one allocation `GetId` makes per device is freed right
        // after it is read.
        unsafe {
            let _com = ComGuard::new()?;
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| Error::Device(format!("creating the device enumerator: {e}")))?;

            let mut devices = Vec::new();
            for (flow, kind) in [
                (eRender, DeviceKind::Playback),
                (eCapture, DeviceKind::Capture),
            ] {
                let default_id = default_endpoint_id(&enumerator, flow);
                collect_endpoints(&enumerator, flow, kind, default_id.as_deref(), &mut devices)?;
            }
            sort_devices(&mut devices);
            Ok(devices)
        }
    }

    /// The device id of the console default endpoint for `flow`.
    ///
    /// `None` covers both "there is no default" (e.g. no active device of that flow at
    /// all, which `GetDefaultAudioEndpoint` reports as an error) and a `GetId` that could
    /// not be read; either way, nothing in `devices` gets marked default rather than the
    /// whole listing failing over a flow that simply has no default endpoint.
    unsafe fn default_endpoint_id(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
    ) -> Option<String> {
        let dev = enumerator.GetDefaultAudioEndpoint(flow, eConsole).ok()?;
        device_id(&dev)
    }

    /// Reads a device's id, freeing the string `GetId` allocates.
    unsafe fn device_id(dev: &IMMDevice) -> Option<String> {
        let id = dev.GetId().ok()?;
        let s = id.to_string().ok();
        CoTaskMemFree(Some(id.as_ptr() as *const _));
        s
    }

    /// Appends every active endpoint of `flow` to `out`, each with its friendly name and
    /// whether its id matches `default_id`.
    unsafe fn collect_endpoints(
        enumerator: &IMMDeviceEnumerator,
        flow: EDataFlow,
        kind: DeviceKind,
        default_id: Option<&str>,
        out: &mut Vec<DeviceInfo>,
    ) -> Result<()> {
        let collection = enumerator
            .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            .map_err(|e| Error::Device(format!("enumerating audio endpoints: {e}")))?;
        let count = collection
            .GetCount()
            .map_err(|e| Error::Device(format!("counting audio endpoints: {e}")))?;
        for i in 0..count {
            let Ok(dev) = collection.Item(i) else {
                continue;
            };
            let name = friendly_name(&dev);
            let is_default = device_id(&dev).as_deref() == default_id;
            out.push(DeviceInfo {
                name,
                kind,
                is_default,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn devices_sort_playback_first_then_by_name() {
        // The window's menus read better grouped, and a stable order means a
        // menu does not reshuffle between openings.
        let mut v = vec![
            DeviceInfo {
                name: "Zebra".into(),
                kind: DeviceKind::Capture,
                is_default: false,
            },
            DeviceInfo {
                name: "Alpha".into(),
                kind: DeviceKind::Capture,
                is_default: false,
            },
            DeviceInfo {
                name: "Beta".into(),
                kind: DeviceKind::Playback,
                is_default: true,
            },
        ];
        sort_devices(&mut v);
        assert_eq!(
            v.iter().map(|d| d.name.as_str()).collect::<Vec<_>>(),
            ["Beta", "Alpha", "Zebra"]
        );
    }

    #[test]
    fn a_node_without_a_media_class_is_not_a_device() {
        assert_eq!(
            kind_from_media_class("Audio/Sink"),
            Some(DeviceKind::Playback)
        );
        assert_eq!(
            kind_from_media_class("Audio/Source"),
            Some(DeviceKind::Capture)
        );
        assert_eq!(kind_from_media_class("Stream/Output/Audio"), None);
        assert_eq!(kind_from_media_class("Video/Source"), None);
        assert_eq!(kind_from_media_class(""), None);
    }

    /// The real enumeration, which needs a session with audio devices.
    /// Run by hand: `cargo test -p pheme-audio --lib devices -- --ignored`
    #[test]
    #[ignore]
    fn the_system_reports_at_least_one_device() {
        let v = list_devices().expect("enumeration");
        assert!(!v.is_empty(), "no audio devices found");
        for d in &v {
            assert!(!d.name.is_empty());
        }
    }
}
