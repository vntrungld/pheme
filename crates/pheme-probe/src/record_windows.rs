//! A low-level keyboard and mouse hook: what reached the system input queue,
//! independent of pheme's own accounting.
//!
//! Unlike pheme's capture, this hook keeps injected input -- pheme's client
//! delivers everything through SendInput, so injected input is exactly what
//! there is to record.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Result};
use pheme_input::keymap::{scancode_to_hid, KEY_NUM_LOCK, KEY_PAUSE, KEY_PRINT_SCREEN};
use pheme_proto::KeyCode;
use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_NUMLOCK, VK_PAUSE, VK_SNAPSHOT};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetCursorPos, PeekMessageW, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, MSLLHOOKSTRUCT,
    PM_REMOVE, WH_KEYBOARD_LL, WH_MOUSE_LL, WM_KEYDOWN, WM_KEYUP, WM_MOUSEMOVE, WM_MOUSEWHEEL,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

use crate::gesture::ProbeEvent;
use crate::record::{windows_wheel, Watch};

struct State {
    events: Vec<ProbeEvent>,
    last: POINT,
}

/// Hook procedures are plain functions, so what they see goes through here.
static STATE: Mutex<Option<State>> = Mutex::new(None);

fn key_of(k: &KBDLLHOOKSTRUCT) -> Option<KeyCode> {
    // The same exceptions pheme's capture makes: these three have no usable
    // scancode in the hook.
    match k.vkCode as u16 {
        v if v == VK_PAUSE.0 => Some(KEY_PAUSE),
        v if v == VK_NUMLOCK.0 => Some(KEY_NUM_LOCK),
        v if v == VK_SNAPSHOT.0 => Some(KEY_PRINT_SCREEN),
        _ => scancode_to_hid(k.scanCode as u16, k.flags.contains(LLKHF_EXTENDED)),
    }
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        let msg = wparam.0 as u32;
        let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
        if let (true, Some(key)) = (down || up, key_of(k)) {
            if let Some(s) = STATE.lock().expect("recorder state").as_mut() {
                s.events.push(if down {
                    ProbeEvent::KeyDown(key)
                } else {
                    ProbeEvent::KeyUp(key)
                });
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let m = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if let Some(s) = STATE.lock().expect("recorder state").as_mut() {
            match wparam.0 as u32 {
                WM_MOUSEMOVE => {
                    let (dx, dy) = (m.pt.x - s.last.x, m.pt.y - s.last.y);
                    s.last = m.pt;
                    s.events.push(ProbeEvent::MoveRel { dx, dy });
                }
                WM_MOUSEWHEEL => s.events.push(windows_wheel(m.mouseData)),
                _ => {}
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

pub fn record(watch: Watch<'_>) -> Result<Vec<ProbeEvent>> {
    let Watch {
        deadline,
        device_hint,
        ready,
        done,
    } = watch;
    if device_hint.is_some() {
        // The hook sees every device at once; there is nothing to narrow.
        return Err(anyhow!("--device-hint has no meaning on Windows"));
    }
    let mut last = POINT::default();
    unsafe { GetCursorPos(&mut last) }.map_err(|e| anyhow!("GetCursorPos: {e}"))?;
    *STATE.lock().expect("recorder state") = Some(State {
        events: Vec::new(),
        last,
    });
    let hooks = unsafe {
        let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)
            .map_err(|e| anyhow!("keyboard hook: {e}"))?;
        match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0) {
            Ok(mouse) => (kbd, mouse),
            Err(e) => {
                let _ = UnhookWindowsHookEx(kbd);
                return Err(anyhow!("mouse hook: {e}"));
            }
        }
    };
    ready();

    // Low-level hooks run only while this thread pumps messages.
    let mut msg = MSG::default();
    loop {
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
        let finished = match STATE.lock().expect("recorder state").as_ref() {
            Some(s) => done(&s.events),
            None => true,
        };
        if finished || Instant::now() >= deadline {
            break;
        }
        std::thread::sleep(Duration::from_millis(5)); // poll interval
    }

    unsafe {
        let _ = UnhookWindowsHookEx(hooks.0);
        let _ = UnhookWindowsHookEx(hooks.1);
    }
    Ok(STATE
        .lock()
        .expect("recorder state")
        .take()
        .map(|s| s.events)
        .unwrap_or_default())
}
