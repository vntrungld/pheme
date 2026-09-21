//! SendInput-based injection.

use pheme_proto::{Button, KeyCode, ScreenInfo};
use tracing::warn;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VK_PAUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    XBUTTON1, XBUTTON2,
};

use crate::keymap::{hid_to_scancode, KEY_PAUSE};
use crate::windows::screens::{ensure_dpi_aware, enum_screens};
use crate::{Error, InputInject, Result};

pub struct WindowsInject {
    screens: Vec<ScreenInfo>,
}

impl Default for WindowsInject {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsInject {
    pub fn new() -> WindowsInject {
        ensure_dpi_aware();
        WindowsInject {
            screens: enum_screens(),
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx,
                dy,
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn send(inputs: &[INPUT]) -> Result<()> {
    let n = unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
    if n as usize == inputs.len() {
        Ok(())
    } else {
        Err(Error::Backend(format!(
            "SendInput injected {n} of {} events",
            inputs.len()
        )))
    }
}

impl InputInject for WindowsInject {
    fn mouse_move_rel(&mut self, dx: i32, dy: i32) -> Result<()> {
        send(&[mouse(dx, dy, 0, MOUSEEVENTF_MOVE)])
    }

    fn mouse_move_abs(&mut self, x: i32, y: i32) -> Result<()> {
        let (vx, vy, vw, vh) = unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        };
        let nx = ((x - vx) as i64 * 65535 / (vw.max(2) - 1) as i64) as i32;
        let ny = ((y - vy) as i64 * 65535 / (vh.max(2) - 1) as i64) as i32;
        send(&[mouse(
            nx,
            ny,
            0,
            MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
        )])
    }

    fn button(&mut self, btn: Button, down: bool) -> Result<()> {
        let (flags, data) = match (btn, down) {
            (Button::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (Button::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
            (Button::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (Button::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (Button::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (Button::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (Button::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1 as i32),
            (Button::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1 as i32),
            (Button::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2 as i32),
            (Button::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2 as i32),
        };
        send(&[mouse(0, 0, data, flags)])
    }

    fn wheel(&mut self, dx: i32, dy: i32) -> Result<()> {
        let mut inputs = Vec::with_capacity(2);
        if dy != 0 {
            inputs.push(mouse(0, 0, dy, MOUSEEVENTF_WHEEL));
        }
        if dx != 0 {
            inputs.push(mouse(0, 0, dx, MOUSEEVENTF_HWHEEL));
        }
        if inputs.is_empty() {
            return Ok(());
        }
        send(&inputs)
    }

    fn key(&mut self, code: KeyCode, down: bool) -> Result<()> {
        let up = if down {
            KEYBD_EVENT_FLAGS(0)
        } else {
            KEYEVENTF_KEYUP
        };
        let ki = if code == KEY_PAUSE {
            // Pause has an E1 prefix that KEYEVENTF_SCANCODE cannot express; use the virtual key.
            KEYBDINPUT {
                wVk: VK_PAUSE,
                wScan: 0,
                dwFlags: up,
                time: 0,
                dwExtraInfo: 0,
            }
        } else {
            let Some((sc, ext)) = hid_to_scancode(code) else {
                warn!(?code, "no scancode mapping; key dropped");
                return Ok(());
            };
            let mut flags = KEYEVENTF_SCANCODE | up;
            if ext {
                flags |= KEYEVENTF_EXTENDEDKEY;
            }
            KEYBDINPUT {
                wVk: Default::default(),
                wScan: sc,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            }
        };
        send(&[INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 { ki },
        }])
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }
}
