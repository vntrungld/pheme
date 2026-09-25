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

/// The Windows half of this lands in the next task; a `todo!()` behind this
/// `cfg` is what keeps the workspace compiling on Windows in the meantime
/// without silently claiming an empty device list.
#[cfg(target_os = "windows")]
pub fn list_devices() -> crate::Result<Vec<DeviceInfo>> {
    todo!("Windows audio device enumeration lands in the next task")
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

    use pipewire as pw;

    use super::{kind_from_media_class, sort_devices, DeviceInfo, DeviceKind};
    use crate::{Error, Result};

    /// A node global as reported by the registry, before it is known whether
    /// it is the default. `node_name` (not the display `name`, which prefers
    /// `node.description`) is what `default.audio.sink`/`default.audio.source`
    /// metadata refers to.
    struct RawNode {
        name: String,
        kind: DeviceKind,
        node_name: String,
    }

    pub(super) fn list_devices() -> Result<Vec<DeviceInfo>> {
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

        let quit_loop = mainloop.clone();
        let core_for_done = core.clone();
        let phase_for_done = phase.clone();
        let pending_for_done = pending.clone();
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
            .register();

        mainloop.run();

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
