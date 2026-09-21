//! Monitor geometry on Linux: RandR under X11, wl_output under Wayland.

use pheme_proto::ScreenInfo;
use tracing::warn;
use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;

use crate::{Error, Result};

pub fn x11_screens(conn: &impl Connection, root: u32) -> Result<Vec<ScreenInfo>> {
    let be = |e: x11rb::errors::ConnectionError| Error::Backend(e.to_string());
    let re = |e: x11rb::errors::ReplyError| Error::Backend(e.to_string());
    let res = conn
        .randr_get_screen_resources_current(root)
        .map_err(be)?
        .reply()
        .map_err(re)?;
    let primary = conn
        .randr_get_output_primary(root)
        .map_err(be)?
        .reply()
        .map_err(re)?
        .output;
    let mut out = Vec::new();
    for crtc in res.crtcs {
        let info = conn
            .randr_get_crtc_info(crtc, res.config_timestamp)
            .map_err(be)?
            .reply()
            .map_err(re)?;
        if info.width == 0 || info.height == 0 {
            continue;
        }
        out.push(ScreenInfo {
            x: info.x as i32,
            y: info.y as i32,
            w: info.width as u32,
            h: info.height as u32,
            primary: info.outputs.contains(&primary),
        });
    }
    if out.is_empty() {
        return Err(Error::Backend("RandR reported no active CRTCs".into()));
    }
    Ok(out)
}

mod wl {
    use pheme_proto::ScreenInfo;
    use wayland_client::protocol::{wl_output, wl_registry};
    use wayland_client::{Connection, Dispatch, QueueHandle};

    #[derive(Default, Clone)]
    struct Out {
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        done: bool,
    }

    #[derive(Default)]
    struct State {
        outputs: Vec<Out>,
    }

    impl Dispatch<wl_registry::WlRegistry, ()> for State {
        fn event(
            state: &mut Self,
            registry: &wl_registry::WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            if let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            {
                if interface == "wl_output" {
                    let idx = state.outputs.len();
                    state.outputs.push(Out::default());
                    registry.bind::<wl_output::WlOutput, usize, Self>(
                        name,
                        version.min(4),
                        qh,
                        idx,
                    );
                }
            }
        }
    }

    impl Dispatch<wl_output::WlOutput, usize> for State {
        fn event(
            state: &mut Self,
            _: &wl_output::WlOutput,
            event: wl_output::Event,
            idx: &usize,
            _: &Connection,
            _: &QueueHandle<Self>,
        ) {
            let o = &mut state.outputs[*idx];
            match event {
                wl_output::Event::Geometry { x, y, .. } => {
                    o.x = x;
                    o.y = y;
                }
                wl_output::Event::Mode {
                    flags,
                    width,
                    height,
                    ..
                } => {
                    if flags
                        .into_result()
                        .map(|f| f.contains(wl_output::Mode::Current))
                        .unwrap_or(false)
                    {
                        o.w = width;
                        o.h = height;
                    }
                }
                wl_output::Event::Done => o.done = true,
                _ => {}
            }
        }
    }

    pub fn screens() -> Result<Vec<ScreenInfo>, String> {
        let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
        let display = conn.display();
        let mut queue = conn.new_event_queue::<State>();
        let qh = queue.handle();
        let _registry = display.get_registry(&qh, ());
        let mut state = State::default();
        // Two roundtrips: one for the globals, one for each output's events.
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        let out: Vec<ScreenInfo> = state
            .outputs
            .iter()
            .filter(|o| o.done && o.w > 0 && o.h > 0)
            .enumerate()
            .map(|(i, o)| ScreenInfo {
                x: o.x,
                y: o.y,
                w: o.w as u32,
                h: o.h as u32,
                primary: i == 0,
            })
            .collect();
        if out.is_empty() {
            return Err("no wl_output reported a current mode".into());
        }
        Ok(out)
    }
}

pub fn wayland_screens() -> Result<Vec<ScreenInfo>> {
    wl::screens().map_err(Error::Backend)
}

/// Best-effort screen list for the current session.
pub fn detect_screens() -> Vec<ScreenInfo> {
    let fallback = || {
        warn!("could not query monitors; assuming one 1920x1080 screen");
        vec![ScreenInfo {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
            primary: true,
        }]
    };
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return wayland_screens().unwrap_or_else(|e| {
            warn!("wayland screens: {e}");
            fallback()
        });
    }
    if std::env::var_os("DISPLAY").is_some() {
        match x11rb::connect(None) {
            Ok((conn, screen_num)) => {
                let root = conn.setup().roots[screen_num].root;
                return x11_screens(&conn, root).unwrap_or_else(|e| {
                    warn!("x11 screens: {e}");
                    fallback()
                });
            }
            Err(e) => warn!("x11 connect: {e}"),
        }
    }
    fallback()
}
