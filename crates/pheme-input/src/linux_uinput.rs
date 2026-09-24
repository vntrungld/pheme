//! Injection through /dev/uinput: works under X11, Wayland and on the console.

use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode as EvKey, PropType,
    RelativeAxisCode, UinputAbsSetup,
};
use pheme_core::Rect;
use pheme_proto::{Button, KeyCode, ScreenInfo};
use tracing::{info, warn};

use crate::keymap::{hid_to_evdev, table::TABLE};
use crate::{Error, InputInject, Result};

pub(crate) const BTN_LEFT: u16 = 0x110;
pub(crate) const BTN_RIGHT: u16 = 0x111;
pub(crate) const BTN_MIDDLE: u16 = 0x112;
pub(crate) const BTN_SIDE: u16 = 0x113;
pub(crate) const BTN_EXTRA: u16 = 0x114;

pub struct UinputInject {
    dev: VirtualDevice,
    screens: Vec<ScreenInfo>,
    bounds: Rect,
    wheel_acc: (i32, i32),
}

const ENODEV: i32 = 19;

fn io_err(e: std::io::Error) -> Error {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        Error::Permission(
            "cannot open /dev/uinput; run `pheme setup` (udev rule + input group) and log in again"
                .into(),
        )
    } else if e.kind() == std::io::ErrorKind::NotFound || e.raw_os_error() == Some(ENODEV) {
        Error::Backend(
            "/dev/uinput is missing: run `sudo modprobe uinput` (pheme setup also persists it)"
                .into(),
        )
    } else {
        Error::Backend(format!("uinput: {e}"))
    }
}

impl UinputInject {
    pub fn new() -> Result<UinputInject> {
        Self::with_screens(crate::linux_screens::detect_screens())
    }

    pub fn with_screens(screens: Vec<ScreenInfo>) -> Result<UinputInject> {
        let bounds = Rect::bounds(&screens);
        let mut keys = AttributeSet::<EvKey>::new();
        for e in TABLE {
            keys.insert(EvKey::new(e.evdev));
        }
        for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
            keys.insert(EvKey::new(b));
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for a in [
            RelativeAxisCode::REL_X,
            RelativeAxisCode::REL_Y,
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            rel.insert(a);
        }
        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_X,
            AbsInfo::new(0, 0, bounds.w - 1, 0, 0, 1),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisCode::ABS_Y,
            AbsInfo::new(0, 0, bounds.h - 1, 0, 0, 1),
        );
        let mut props = AttributeSet::<PropType>::new();
        props.insert(PropType::POINTER);
        let dev = VirtualDevice::builder()
            .map_err(io_err)?
            .name("Pheme Virtual Input")
            .with_keys(&keys)
            .map_err(io_err)?
            .with_relative_axes(&rel)
            .map_err(io_err)?
            .with_absolute_axis(&abs_x)
            .map_err(io_err)?
            .with_absolute_axis(&abs_y)
            .map_err(io_err)?
            .with_properties(&props)
            .map_err(io_err)?
            .build()
            .map_err(io_err)?;
        info!(?bounds, "uinput device created");
        Ok(UinputInject {
            dev,
            screens,
            bounds,
            wheel_acc: (0, 0),
        })
    }

    fn emit(&mut self, events: &[InputEvent]) -> Result<()> {
        self.dev
            .emit(events)
            .map_err(|e| Error::Backend(format!("uinput emit: {e}")))
    }
}

pub(crate) fn button_code(b: Button) -> u16 {
    match b {
        Button::Left => BTN_LEFT,
        Button::Right => BTN_RIGHT,
        Button::Middle => BTN_MIDDLE,
        Button::Back => BTN_SIDE,
        Button::Forward => BTN_EXTRA,
    }
}

impl InputInject for UinputInject {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        self.emit(&[
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_X.0, dx),
            InputEvent::new(EventType::RELATIVE.0, RelativeAxisCode::REL_Y.0, dy),
        ])
    }

    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        let ax = (x - self.bounds.x).clamp(0, self.bounds.w - 1);
        let ay = (y - self.bounds.y).clamp(0, self.bounds.h - 1);
        self.emit(&[
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, ax),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, ay),
        ])
    }

    fn button(&mut self, btn: Button, down: bool) -> Result<()> {
        self.emit(&[InputEvent::new(
            EventType::KEY.0,
            button_code(btn),
            down as i32,
        )])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut ev = Vec::with_capacity(4);
        if dy != 0 {
            ev.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_WHEEL_HI_RES.0,
                dy,
            ));
            self.wheel_acc.1 += dy;
            let notches = self.wheel_acc.1 / 120;
            if notches != 0 {
                ev.push(InputEvent::new(
                    EventType::RELATIVE.0,
                    RelativeAxisCode::REL_WHEEL.0,
                    notches,
                ));
                self.wheel_acc.1 -= notches * 120;
            }
        }
        if dx != 0 {
            ev.push(InputEvent::new(
                EventType::RELATIVE.0,
                RelativeAxisCode::REL_HWHEEL_HI_RES.0,
                dx,
            ));
            self.wheel_acc.0 += dx;
            let notches = self.wheel_acc.0 / 120;
            if notches != 0 {
                ev.push(InputEvent::new(
                    EventType::RELATIVE.0,
                    RelativeAxisCode::REL_HWHEEL.0,
                    notches,
                ));
                self.wheel_acc.0 -= notches * 120;
            }
        }
        if ev.is_empty() {
            return Ok(());
        }
        self.emit(&ev)
    }

    fn key(&mut self, code: KeyCode, down: bool) -> Result<()> {
        let Some(ev) = hid_to_evdev(code) else {
            warn!(?code, "no evdev mapping; key dropped");
            return Ok(());
        };
        self.emit(&[InputEvent::new(EventType::KEY.0, ev, down as i32)])
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }
}
