//! WH_KEYBOARD_LL / WH_MOUSE_LL hooks for observe+grab, Raw Input for unaccelerated deltas.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crossbeam_channel::Sender;
use pheme_core::{CaptureEvent, Rect};
use pheme_proto::{Button, ScreenInfo};
use tracing::{debug, error, info, warn};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{
    GetLastError, ERROR_CLASS_ALREADY_EXISTS, HMODULE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_NUMLOCK, VK_PAUSE, VK_SNAPSHOT};
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, HRAWINPUT, MOUSE_MOVE_ABSOLUTE, RAWINPUT,
    RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK, RID_INPUT, RIM_TYPEMOUSE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, ClipCursor, CreateCursor, CreateWindowExW, DefWindowProcW, DestroyCursor,
    DestroyWindow, DispatchMessageW, GetMessageW, PostThreadMessageW, RegisterClassW, SetCursorPos,
    SetSystemCursor, SetWindowsHookExW, SystemParametersInfoW, TranslateMessage,
    UnhookWindowsHookEx, UnregisterClassW, HHOOK, HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED,
    LLKHF_INJECTED, LLMHF_INJECTED, MSG, MSLLHOOKSTRUCT, OCR_NORMAL, SPI_SETCURSORS,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WH_KEYBOARD_LL, WH_MOUSE_LL, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_APP, WM_INPUT, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN,
    WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSW,
};

use crate::keymap::{scancode_to_hid, KEY_NUM_LOCK, KEY_PAUSE, KEY_PRINT_SCREEN};
use crate::windows::screens::{ensure_dpi_aware, enum_screens};
use crate::{CaptureMode, Error, InputCapture, Result};

const WM_SET_MODE: u32 = WM_APP + 1;
const WM_STOP: u32 = WM_APP + 2;

const WNDCLASS_NAME: PCWSTR = w!("PhemeRawInput");

/// How long `set_mode` waits for the hook thread to apply (or fail) a mode change.
const MODE_CHANGE_TIMEOUT: Duration = Duration::from_secs(1);

struct Hooks {
    tx: Sender<CaptureEvent>,
    grab: bool,
    /// Where the hook thread reports the result of the pending `WM_SET_MODE`, if any.
    ack: Option<mpsc::Sender<Result<()>>>,
}

/// Shared with the hook procedures (they are plain functions without a `self`).
static HOOKS: Mutex<Option<Hooks>> = Mutex::new(None);

/// Count of events dropped because the channel was full; logged at a decaying rate so a
/// stalled consumer does not spam the log from inside the hook procedures.
static DROPPED: AtomicU64 = AtomicU64::new(0);

fn send(ev: CaptureEvent) {
    if let Some(h) = HOOKS.lock().unwrap().as_ref() {
        if h.tx.try_send(ev).is_err() {
            let n = DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
            if n.is_power_of_two() || n % 1000 == 0 {
                warn!(dropped = n, "capture channel full; dropping event");
            }
        }
    }
}

fn set_grab_flag(on: bool) {
    if let Some(h) = HOOKS.lock().unwrap().as_mut() {
        h.grab = on;
    }
}

/// Takes the pending `set_mode` acknowledgement sender out of `HOOKS`, if any.
fn take_ack() -> Option<mpsc::Sender<Result<()>>> {
    HOOKS.lock().unwrap().as_mut().and_then(|h| h.ack.take())
}

fn grabbing() -> bool {
    HOOKS
        .lock()
        .unwrap()
        .as_ref()
        .map(|h| h.grab)
        .unwrap_or(false)
}

unsafe extern "system" fn keyboard_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = &*(lparam.0 as *const KBDLLHOOKSTRUCT);
        if !k.flags.contains(LLKHF_INJECTED) {
            let msg = wparam.0 as u32;
            let down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if down || up {
                let hid = match k.vkCode as u16 {
                    v if v == VK_PAUSE.0 => Some(KEY_PAUSE),
                    v if v == VK_NUMLOCK.0 => Some(KEY_NUM_LOCK),
                    v if v == VK_SNAPSHOT.0 => Some(KEY_PRINT_SCREEN),
                    _ => scancode_to_hid(k.scanCode as u16, k.flags.contains(LLKHF_EXTENDED)),
                };
                match hid {
                    Some(code) => send(CaptureEvent::Key { code, down }),
                    None => debug!(vk = k.vkCode, sc = k.scanCode, "unmapped key"),
                }
            }
            if grabbing() {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn mouse_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let m = &*(lparam.0 as *const MSLLHOOKSTRUCT);
        if m.flags & LLMHF_INJECTED == 0 {
            let msg = wparam.0 as u32;
            let grab = grabbing();
            match msg {
                WM_MOUSEMOVE if !grab => send(CaptureEvent::MotionAbs {
                    x: m.pt.x,
                    y: m.pt.y,
                }),
                WM_MOUSEMOVE => {} // deltas come from Raw Input while grabbing
                _ if !grab => {}
                WM_LBUTTONDOWN | WM_LBUTTONUP => send(CaptureEvent::Button {
                    btn: Button::Left,
                    down: msg == WM_LBUTTONDOWN,
                }),
                WM_RBUTTONDOWN | WM_RBUTTONUP => send(CaptureEvent::Button {
                    btn: Button::Right,
                    down: msg == WM_RBUTTONDOWN,
                }),
                WM_MBUTTONDOWN | WM_MBUTTONUP => send(CaptureEvent::Button {
                    btn: Button::Middle,
                    down: msg == WM_MBUTTONDOWN,
                }),
                WM_XBUTTONDOWN | WM_XBUTTONUP => {
                    let btn = if (m.mouseData >> 16) & 0xFFFF == 1 {
                        Button::Back
                    } else {
                        Button::Forward
                    };
                    send(CaptureEvent::Button {
                        btn,
                        down: msg == WM_XBUTTONDOWN,
                    });
                }
                WM_MOUSEWHEEL => send(CaptureEvent::Wheel {
                    dx: 0,
                    dy: ((m.mouseData >> 16) as u16 as i16) as i32,
                }),
                WM_MOUSEHWHEEL => send(CaptureEvent::Wheel {
                    dx: ((m.mouseData >> 16) as u16 as i16) as i32,
                    dy: 0,
                }),
                _ => {}
            }
            if grab {
                return LRESULT(1);
            }
        }
    }
    CallNextHookEx(None, code, wparam, lparam)
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_INPUT && grabbing() {
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let mut raw = RAWINPUT::default();
        let got = GetRawInputData(
            HRAWINPUT(lparam.0 as *mut _),
            RID_INPUT,
            Some(&mut raw as *mut _ as *mut _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if got != u32::MAX && raw.header.dwType == RIM_TYPEMOUSE.0 {
            let mouse = raw.data.mouse;
            if mouse.usFlags.0 & MOUSE_MOVE_ABSOLUTE.0 == 0
                && (mouse.lLastX != 0 || mouse.lLastY != 0)
            {
                send(CaptureEvent::MotionRel {
                    dx: mouse.lLastX,
                    dy: mouse.lLastY,
                });
            }
        }
        // Fall through to DefWindowProc as the Raw Input docs require, for cleanup.
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

pub struct WindowsCapture {
    screens: Vec<ScreenInfo>,
    thread_id: Option<u32>,
    thread: Option<JoinHandle<()>>,
}

impl Default for WindowsCapture {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsCapture {
    pub fn new() -> WindowsCapture {
        ensure_dpi_aware();
        WindowsCapture {
            screens: enum_screens(),
            thread_id: None,
            thread: None,
        }
    }
}

impl Drop for WindowsCapture {
    /// Ensures a capture dropped while grabbed still restores the system cursor, unclips it
    /// and unhooks; `stop()` is idempotent so this is safe even after an explicit `stop()`.
    fn drop(&mut self) {
        self.stop();
    }
}

/// Everything that lives on the hook thread.
struct HookThread {
    hinst: HMODULE,
    hwnd: HWND,
    kbd: HHOOK,
    mouse: HHOOK,
    center: (i32, i32),
    hidden: bool,
}

impl HookThread {
    unsafe fn install(center: (i32, i32)) -> Result<HookThread> {
        let hinst = GetModuleHandleW(None).map_err(|e| Error::Backend(e.to_string()))?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst.into(),
            lpszClassName: WNDCLASS_NAME,
            ..Default::default()
        };
        // The window class is process-global; `uninstall` unregisters it, but treat
        // "already registered" (e.g. a prior instance that failed partway through install,
        // so `uninstall` never ran) as success too, so a retried or fresh `start()` after
        // `stop()` does not fail with ERROR_CLASS_ALREADY_EXISTS.
        if RegisterClassW(&class) == 0 {
            let err = GetLastError();
            if err != ERROR_CLASS_ALREADY_EXISTS {
                return Err(Error::Backend(format!("RegisterClassW failed: {err:?}")));
            }
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WNDCLASS_NAME,
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(hinst.into()),
            None,
        )
        .map_err(|e| Error::Backend(format!("CreateWindowExW: {e}")))?;
        let rid = RAWINPUTDEVICE {
            usUsagePage: 1,
            usUsage: 2,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: hwnd,
        };
        RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32)
            .map_err(|e| Error::Backend(format!("RegisterRawInputDevices: {e}")))?;
        let kbd = SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook), None, 0)
            .map_err(|e| Error::Backend(format!("keyboard hook: {e}")))?;
        let mouse = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook), None, 0)
            .map_err(|e| Error::Backend(format!("mouse hook: {e}")))?;
        Ok(HookThread {
            hinst,
            hwnd,
            kbd,
            mouse,
            center,
            hidden: false,
        })
    }

    /// Applies a mode. Never holds the `HOOKS` lock while calling into Win32 (the hook
    /// procedures take that lock on the same thread's message dispatch). On `Err` the
    /// previous mode is restored so a failed grab leaves nothing clipped or swallowed.
    unsafe fn set_grab(&mut self, on: bool) -> Result<()> {
        set_grab_flag(on);
        let r = if on {
            let (cx, cy) = self.center;
            let rect = RECT {
                left: cx,
                top: cy,
                right: cx + 1,
                bottom: cy + 1,
            };
            let r = SetCursorPos(cx, cy)
                .map_err(|e| Error::Backend(format!("SetCursorPos: {e}")))
                .and_then(|_| {
                    ClipCursor(Some(&rect)).map_err(|e| Error::Backend(format!("ClipCursor: {e}")))
                });
            match r {
                Ok(()) => {
                    self.hide_cursor();
                    Ok(())
                }
                Err(e) => {
                    set_grab_flag(false);
                    let _ = ClipCursor(None);
                    Err(e)
                }
            }
        } else {
            let r = ClipCursor(None).map_err(|e| Error::Backend(format!("ClipCursor(None): {e}")));
            // Show the cursor even if unclipping failed: staying invisible is worse.
            self.show_cursor();
            if r.is_err() {
                set_grab_flag(true);
            }
            r
        };
        match &r {
            Ok(()) => debug!(grab = on, "mode applied"),
            Err(e) => error!(grab = on, "mode change failed: {e}"),
        }
        r
    }

    /// Replaces the arrow cursor with a blank one system-wide (hooks stop it from moving anyway).
    unsafe fn hide_cursor(&mut self) {
        if self.hidden {
            return;
        }
        let and_mask = [0xFFu8; 32 * 32 / 8];
        let xor_mask = [0x00u8; 32 * 32 / 8];
        match CreateCursor(
            None,
            0,
            0,
            32,
            32,
            and_mask.as_ptr() as *const _,
            xor_mask.as_ptr() as *const _,
        ) {
            Ok(blank) => {
                // SetSystemCursor takes ownership of the handle on success only; on failure
                // the handle is still ours to free.
                if let Err(e) = SetSystemCursor(blank, OCR_NORMAL) {
                    warn!("SetSystemCursor failed: {e}");
                    if let Err(e) = DestroyCursor(blank) {
                        debug!("DestroyCursor failed: {e}");
                    }
                } else {
                    self.hidden = true;
                }
            }
            Err(e) => warn!("CreateCursor failed: {e}"),
        }
    }

    unsafe fn show_cursor(&mut self) {
        if self.hidden {
            let _ = SystemParametersInfoW(
                SPI_SETCURSORS,
                0,
                None,
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            );
            self.hidden = false;
        }
    }

    unsafe fn uninstall(mut self) {
        if grabbing() {
            let _ = self.set_grab(false);
        }
        let _ = UnhookWindowsHookEx(self.kbd);
        let _ = UnhookWindowsHookEx(self.mouse);
        if let Err(e) = DestroyWindow(self.hwnd) {
            debug!("DestroyWindow failed: {e}");
        }
        // Unregister the class so a later `start()` (retry, or after `stop()`) does not hit
        // ERROR_CLASS_ALREADY_EXISTS; window classes are process-global and otherwise outlive
        // this HookThread.
        if let Err(e) = UnregisterClassW(WNDCLASS_NAME, Some(self.hinst.into())) {
            debug!("UnregisterClassW failed: {e}");
        }
        *HOOKS.lock().unwrap() = None;
    }
}

fn hook_thread_main(
    tx: Sender<CaptureEvent>,
    center: (i32, i32),
    ready: std::sync::mpsc::Sender<Result<u32>>,
) {
    unsafe {
        let ht = match HookThread::install(center) {
            Ok(ht) => ht,
            Err(e) => {
                let _ = ready.send(Err(e));
                return;
            }
        };
        *HOOKS.lock().unwrap() = Some(Hooks {
            tx,
            grab: false,
            ack: None,
        });
        let _ = ready.send(Ok(GetCurrentThreadId()));
        let mut ht = ht;
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if msg.hwnd.0.is_null() {
                match msg.message {
                    WM_SET_MODE => {
                        // Take the sender out (and release the lock) before Win32 calls.
                        let ack = take_ack();
                        let r = ht.set_grab(msg.wParam.0 == 1);
                        if let Some(ack) = ack {
                            // The caller may have given up (timeout); nothing to do then.
                            let _ = ack.send(r);
                        }
                    }
                    WM_STOP => break,
                    _ => {}
                }
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        ht.uninstall();
        info!("hook thread exited");
    }
}

impl InputCapture for WindowsCapture {
    fn start(&mut self, tx: Sender<CaptureEvent>) -> Result<()> {
        if self.thread.is_some() {
            return Err(Error::Backend("already started".into()));
        }
        let center = Rect::bounds(&self.screens).center();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("pheme-hooks".into())
            .spawn(move || hook_thread_main(tx, center, ready_tx))
            .map_err(|e| Error::Backend(e.to_string()))?;
        let id = ready_rx
            .recv()
            .map_err(|_| Error::Backend("hook thread died".into()))??;
        self.thread_id = Some(id);
        self.thread = Some(thread);
        Ok(())
    }

    /// Synchronous (see the trait contract): posts the request to the hook thread and
    /// waits for it to report the actual `ClipCursor`/`SetCursorPos` result, so a
    /// `warp_cursor` issued after `set_mode(Observe)` is never clamped by a still-active clip.
    fn set_mode(&mut self, mode: CaptureMode) -> Result<()> {
        let id = self
            .thread_id
            .ok_or_else(|| Error::Backend("not started".into()))?;
        let (ack_tx, ack_rx) = mpsc::channel::<Result<()>>();
        {
            let mut hooks = HOOKS.lock().unwrap();
            let h = hooks
                .as_mut()
                .ok_or_else(|| Error::Backend("hook thread is gone".into()))?;
            h.ack = Some(ack_tx);
        }
        let grab = matches!(mode, CaptureMode::Grab) as usize;
        if let Err(e) = unsafe { PostThreadMessageW(id, WM_SET_MODE, WPARAM(grab), LPARAM(0)) } {
            take_ack();
            return Err(Error::Backend(format!("PostThreadMessageW: {e}")));
        }
        match ack_rx.recv_timeout(MODE_CHANGE_TIMEOUT) {
            Ok(r) => r,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                take_ack();
                Err(Error::Backend("mode change timed out".into()))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Error::Backend(
                "hook thread exited before applying the mode change".into(),
            )),
        }
    }

    fn warp_cursor(&mut self, x: i32, y: i32) -> Result<()> {
        unsafe { SetCursorPos(x, y) }.map_err(|e| Error::Backend(e.to_string()))
    }

    fn screens(&self) -> Vec<ScreenInfo> {
        self.screens.clone()
    }

    fn stop(&mut self) {
        if let Some(id) = self.thread_id.take() {
            unsafe {
                if let Err(e) = PostThreadMessageW(id, WM_STOP, WPARAM(0), LPARAM(0)) {
                    error!("PostThreadMessageW(WM_STOP): {e}");
                }
            }
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}
