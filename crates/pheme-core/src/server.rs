//! Server-side state machine: decides when input leaves for a client and comes back.

use std::collections::{BTreeSet, HashMap};

use pheme_proto::{Button, KeyCode, Modifiers, Msg, ScreenInfo};
use tracing::debug;

use crate::geometry::{
    edge_segment, on_edge, project_entry, project_exit, EdgeSegment, Rect, Side,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ClientPlacement {
    pub name: String,
    pub side: Side,
    pub span: (f32, f32),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub server_screens: Vec<ScreenInfo>,
    pub clients: Vec<ClientPlacement>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hotkeys {
    pub lock: Option<KeyCode>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CaptureEvent {
    MotionAbs {
        x: i32,
        y: i32,
    },
    MotionRel {
        dx: i32,
        dy: i32,
    },
    Button {
        btn: Button,
        down: bool,
    },
    Wheel {
        dx: i32,
        dy: i32,
    },
    Key {
        code: KeyCode,
        down: bool,
    },
    /// The backend stopped capturing without the pointer leaving the client — the
    /// compositor ended it. Only the Wayland backend produces this; the X11 and
    /// Windows backends keep capturing until they are told to stop.
    CaptureEnded,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SendControl(Msg),
    SendDatagram(Msg),
    Grab,
    /// Stop capturing and put the pointer at (x, y). The position is part of the
    /// action because it always was: every `Ungrab` was followed by a `WarpCursor`
    /// with these exact coordinates, and a backend that releases by naming a
    /// position (the InputCapture portal) cannot depend on that pairing silently.
    Ungrab {
        x: i32,
        y: i32,
    },
    WarpCursor {
        x: i32,
        y: i32,
    },
    SetLocked(bool),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Active {
    Local,
    Remote(String),
}

struct Remote {
    name: String,
    seg: EdgeSegment,
    client: Rect,
    /// Virtual pointer position relative to the client rect origin.
    vx: i32,
    vy: i32,
}

pub struct ServerCore {
    layout: Layout,
    hotkeys: Hotkeys,
    server: Rect,
    /// Connected clients by name with their screen bounds.
    connected: HashMap<String, Rect>,
    remote: Option<Remote>,
    last_pos: Option<(i32, i32)>,
    held: BTreeSet<KeyCode>,
    pub(crate) locked: bool,
    seq: u32,
}

impl ServerCore {
    pub fn new(layout: Layout, hotkeys: Hotkeys) -> Self {
        let server = Rect::bounds(&layout.server_screens);
        ServerCore {
            layout,
            hotkeys,
            server,
            connected: HashMap::new(),
            remote: None,
            last_pos: None,
            held: BTreeSet::new(),
            locked: false,
            seq: 0,
        }
    }

    pub fn active(&self) -> Active {
        match &self.remote {
            Some(r) => Active::Remote(r.name.clone()),
            None => Active::Local,
        }
    }

    pub fn locked(&self) -> bool {
        self.locked
    }

    /// Toggles the input lock. The X11 and Windows backends reach this through the
    /// hotkey in `on_event`; on Wayland the GlobalShortcuts portal calls it directly,
    /// because a Wayland server sees no keys while local.
    pub fn toggle_lock(&mut self) -> Vec<Action> {
        self.locked = !self.locked;
        vec![Action::SetLocked(self.locked)]
    }

    pub fn client_connected(&mut self, name: &str, screens: Vec<ScreenInfo>) -> Vec<Action> {
        debug!(name, ?screens, "client connected");
        self.connected
            .insert(name.to_string(), Rect::bounds(&screens));
        Vec::new()
    }

    /// The edges a crossing may start a capture on: one per placement whose client is
    /// currently connected.
    ///
    /// Edges without a connected client are deliberately excluded. A backend that
    /// declares them (the InputCapture portal) would have the compositor stop the
    /// pointer at that edge, and the core would then decline the switch — so the
    /// pointer would snag on a screen edge that leads nowhere.
    pub fn capture_edges(&self) -> Vec<(Side, (f32, f32))> {
        self.layout
            .clients
            .iter()
            .filter(|p| self.connected.contains_key(&p.name))
            .map(|p| (p.side, p.span))
            .collect()
    }

    pub fn client_disconnected(&mut self, name: &str) -> Vec<Action> {
        self.connected.remove(name);
        match &self.remote {
            Some(r) if r.name == name => {
                self.remote = None;
                let (x, y) = self.server.center();
                self.last_pos = Some((x, y));
                vec![Action::Ungrab { x, y }]
            }
            _ => Vec::new(),
        }
    }

    /// Cancels a switch whose `Grab` failed: returns to Local as if the crossing never
    /// happened. No `Leave` is sent because `Enter` has not been sent yet (`Grab` is the
    /// first action of a switch). Returns nothing when already Local.
    pub fn abort_switch(&mut self) -> Vec<Action> {
        if self.remote.take().is_none() {
            return Vec::new();
        }
        let (x, y) = self.server.center();
        self.last_pos = Some((x, y));
        debug!("switch aborted; back to local");
        vec![Action::WarpCursor { x, y }]
    }

    /// Returns to Local because the capture ended without the pointer leaving the
    /// client — the compositor ended it on its own, which the InputCapture portal
    /// permits at any time.
    ///
    /// Unlike `client_disconnected` this sends `Msg::Leave`: the client is still
    /// connected, and without it every key held at that instant stays held there.
    /// Unlike `abort_switch` the switch did happen, so `Enter` was already sent.
    pub fn release_remote(&mut self) -> Vec<Action> {
        if self.remote.take().is_none() {
            return Vec::new();
        }
        let (x, y) = self.server.center();
        self.last_pos = Some((x, y));
        let seq = self.next_seq();
        debug!("capture ended by the compositor; back to local");
        vec![
            Action::SendControl(Msg::Leave { seq }),
            Action::Ungrab { x, y },
        ]
    }

    pub fn on_event(&mut self, ev: CaptureEvent) -> Vec<Action> {
        if let CaptureEvent::CaptureEnded = ev {
            return self.release_remote();
        }
        if let CaptureEvent::Key { code, down } = ev {
            if Some(code) == self.hotkeys.lock {
                if down {
                    return self.toggle_lock();
                }
                return Vec::new();
            }
            if down {
                self.held.insert(code);
            } else {
                self.held.remove(&code);
            }
        }
        if self.remote.is_some() {
            self.on_remote_event(ev)
        } else {
            self.on_local_event(ev)
        }
    }

    fn next_seq(&mut self) -> u32 {
        self.seq = self.seq.wrapping_add(1);
        self.seq
    }

    fn on_local_event(&mut self, ev: CaptureEvent) -> Vec<Action> {
        let CaptureEvent::MotionAbs { x, y } = ev else {
            return Vec::new();
        };
        let prev = self.last_pos.replace((x, y));
        if self.locked {
            return Vec::new();
        }
        for placement in &self.layout.clients {
            let Some(client) = self.connected.get(&placement.name).copied() else {
                continue;
            };
            let seg = edge_segment(&self.server, placement.side, placement.span);
            let Some(along) = on_edge(&self.server, placement.side, x, y) else {
                continue;
            };
            if !seg.contains(along) {
                continue;
            }
            if let Some((px, py)) = prev {
                if on_edge(&self.server, placement.side, px, py).is_some() {
                    continue; // sliding along the edge, not crossing it
                }
            }
            let (ex, ey) = project_entry(&seg, along, &client);
            let (cx, cy) = self.server.center();
            let mods = Modifiers::from_held(self.held.iter().copied());
            // Inlined `next_seq()`: a method call here would need `&mut self`, which the
            // borrow checker can't reconcile with the outstanding `&self.layout.clients`
            // loan from the `for` loop. Direct field access is a disjoint borrow.
            self.seq = self.seq.wrapping_add(1);
            let seq = self.seq;
            debug!(client = %placement.name, ex, ey, "entering client");
            self.remote = Some(Remote {
                name: placement.name.clone(),
                seg,
                client,
                vx: ex as i32,
                vy: ey as i32,
            });
            return vec![
                Action::Grab,
                Action::WarpCursor { x: cx, y: cy },
                Action::SendControl(Msg::Enter {
                    seq,
                    x: ex,
                    y: ey,
                    mods,
                }),
            ];
        }
        Vec::new()
    }

    fn on_remote_event(&mut self, ev: CaptureEvent) -> Vec<Action> {
        match ev {
            CaptureEvent::MotionRel { dx, dy } => {
                let seq = self.next_seq();
                let mut actions = vec![Action::SendDatagram(Msg::MouseMove {
                    seq,
                    dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                })];
                let r = self.remote.as_mut().expect("remote");
                r.vx += dx;
                r.vy += dy;
                let leaving = match r.seg.side {
                    Side::Right => r.vx < 0,
                    Side::Left => r.vx >= r.client.w,
                    Side::Bottom => r.vy < 0,
                    Side::Top => r.vy >= r.client.h,
                };
                if leaving && !self.locked {
                    let (x, y) = project_exit(&r.seg, &r.client, r.vx, r.vy, &self.server);
                    debug!(x, y, "leaving client");
                    self.remote = None;
                    self.last_pos = Some((x, y));
                    let seq = self.next_seq();
                    actions.insert(0, Action::SendControl(Msg::Leave { seq }));
                    actions.truncate(1);
                    actions.push(Action::Ungrab { x, y });
                } else {
                    r.vx = r.vx.clamp(0, r.client.w - 1);
                    r.vy = r.vy.clamp(0, r.client.h - 1);
                }
                actions
            }
            CaptureEvent::MotionAbs { .. } => Vec::new(),
            CaptureEvent::Button { btn, down } => {
                let seq = self.next_seq();
                vec![Action::SendControl(Msg::Button { seq, btn, down })]
            }
            CaptureEvent::Wheel { dx, dy } => {
                let seq = self.next_seq();
                vec![Action::SendDatagram(Msg::Wheel {
                    seq,
                    dx: dx.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                    dy: dy.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
                })]
            }
            CaptureEvent::Key { code, down } => {
                let seq = self.next_seq();
                vec![Action::SendControl(Msg::Key { seq, code, down })]
            }
            CaptureEvent::CaptureEnded => {
                unreachable!("on_event handles CaptureEnded before dispatching here")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_proto::{Button, KeyCode, Modifiers, Msg, ScreenInfo};

    const LOCK: KeyCode = KeyCode(0x47); // ScrollLock

    fn screen(w: u32, h: u32) -> Vec<ScreenInfo> {
        vec![ScreenInfo {
            x: 0,
            y: 0,
            w,
            h,
            primary: true,
        }]
    }

    fn core(side: Side, span: (f32, f32)) -> ServerCore {
        let layout = Layout {
            server_screens: screen(1920, 1080),
            clients: vec![ClientPlacement {
                name: "lap".into(),
                side,
                span,
            }],
        };
        let mut c = ServerCore::new(layout, Hotkeys { lock: Some(LOCK) });
        c.client_connected("lap", screen(1000, 500));
        c
    }

    fn enter_right(c: &mut ServerCore) -> Vec<Action> {
        c.on_event(CaptureEvent::MotionAbs { x: 1900, y: 540 });
        c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 })
    }

    fn has_enter(actions: &[Action]) -> Option<(u16, u16, Modifiers)> {
        actions.iter().find_map(|a| match a {
            Action::SendControl(Msg::Enter { x, y, mods, .. }) => Some((*x, *y, *mods)),
            _ => None,
        })
    }

    #[test]
    fn enters_right_client_with_projected_coordinates() {
        let mut c = core(Side::Right, (0.0, 1.0));
        let actions = enter_right(&mut c);
        assert!(matches!(actions[0], Action::Grab));
        assert!(matches!(actions[1], Action::WarpCursor { x: 960, y: 540 }));
        assert_eq!(has_enter(&actions), Some((0, 250, Modifiers(0))));
        assert_eq!(c.active(), Active::Remote("lap".into()));
    }

    #[test]
    fn enters_left_top_bottom() {
        let mut c = core(Side::Left, (0.0, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 5, y: 0 });
        let a = c.on_event(CaptureEvent::MotionAbs { x: 0, y: 0 });
        assert_eq!(has_enter(&a), Some((999, 0, Modifiers(0))));

        let mut c = core(Side::Top, (0.0, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 960, y: 5 });
        let a = c.on_event(CaptureEvent::MotionAbs { x: 960, y: 0 });
        assert_eq!(has_enter(&a), Some((500, 499, Modifiers(0))));

        let mut c = core(Side::Bottom, (0.0, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 0, y: 1000 });
        let a = c.on_event(CaptureEvent::MotionAbs { x: 0, y: 1079 });
        assert_eq!(has_enter(&a), Some((0, 0, Modifiers(0))));
    }

    #[test]
    fn partial_span_only_switches_inside_the_segment() {
        let mut c = core(Side::Right, (0.5, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 1900, y: 100 });
        let a = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 100 });
        assert!(a.is_empty());
        c.on_event(CaptureEvent::MotionAbs { x: 1900, y: 810 });
        let a = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 810 });
        assert_eq!(has_enter(&a), Some((0, 250, Modifiers(0))));
    }

    #[test]
    fn dragging_along_the_edge_does_not_switch() {
        let mut c = core(Side::Right, (0.0, 1.0));
        c.locked = true; // park the pointer on the edge without switching
        c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 100 });
        c.locked = false;
        let a = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 200 });
        assert!(a.is_empty(), "previous event was already on the edge");
    }

    #[test]
    fn first_event_on_the_edge_counts_as_a_crossing() {
        let mut c = core(Side::Right, (0.0, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 100 });
        assert_eq!(c.active(), Active::Remote("lap".into()));
    }

    #[test]
    fn no_client_on_that_edge_or_not_connected() {
        let mut c = core(Side::Right, (0.0, 1.0));
        c.on_event(CaptureEvent::MotionAbs { x: 5, y: 0 });
        assert!(c
            .on_event(CaptureEvent::MotionAbs { x: 0, y: 0 })
            .is_empty());

        let layout = Layout {
            server_screens: screen(1920, 1080),
            clients: vec![ClientPlacement {
                name: "lap".into(),
                side: Side::Right,
                span: (0.0, 1.0),
            }],
        };
        let mut c = ServerCore::new(layout, Hotkeys { lock: None });
        assert!(enter_right(&mut c).is_empty());
    }

    #[test]
    fn remote_motion_forwards_and_leaves_through_the_return_edge() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c); // virtual position (0, 250)
        let a = c.on_event(CaptureEvent::MotionRel { dx: 10, dy: -5 });
        assert!(matches!(
            a[..],
            [Action::SendDatagram(Msg::MouseMove { dx: 10, dy: -5, .. })]
        ));
        // other edges only clamp
        let a = c.on_event(CaptureEvent::MotionRel { dx: 0, dy: -1000 });
        assert_eq!(a.len(), 1);
        assert_eq!(c.active(), Active::Remote("lap".into()));
        // cross back through the client's left edge at vy = 0 → server (1918, 0)
        let a = c.on_event(CaptureEvent::MotionRel { dx: -20, dy: 0 });
        assert!(matches!(a[0], Action::SendControl(Msg::Leave { .. })));
        assert!(matches!(a[1], Action::Ungrab { x: 1918, y: 0 }));
        assert_eq!(c.active(), Active::Local);
        // the warp point counts as the previous position, so the next edge hit is a fresh crossing
        let a = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 0 });
        assert!(has_enter(&a).is_some());
    }

    #[test]
    fn keys_buttons_wheel_are_forwarded_when_remote() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let a = c.on_event(CaptureEvent::Key {
            code: KeyCode(0x04),
            down: true,
        });
        assert!(matches!(
            a[..],
            [Action::SendControl(Msg::Key {
                code: KeyCode(0x04),
                down: true,
                ..
            })]
        ));
        let a = c.on_event(CaptureEvent::Button {
            btn: Button::Left,
            down: true,
        });
        assert!(matches!(
            a[..],
            [Action::SendControl(Msg::Button {
                btn: Button::Left,
                down: true,
                ..
            })]
        ));
        let a = c.on_event(CaptureEvent::Wheel { dx: 0, dy: 120 });
        assert!(matches!(
            a[..],
            [Action::SendDatagram(Msg::Wheel { dx: 0, dy: 120, .. })]
        ));
    }

    #[test]
    fn held_modifiers_are_reported_on_enter() {
        let mut c = core(Side::Right, (0.0, 1.0));
        assert!(c
            .on_event(CaptureEvent::Key {
                code: KeyCode::LEFT_SHIFT,
                down: true
            })
            .is_empty());
        let a = enter_right(&mut c);
        assert_eq!(has_enter(&a).unwrap().2, Modifiers(Modifiers::SHIFT));
    }

    #[test]
    fn lock_blocks_switching_and_leaving() {
        let mut c = core(Side::Right, (0.0, 1.0));
        let a = c.on_event(CaptureEvent::Key {
            code: LOCK,
            down: true,
        });
        assert!(matches!(a[..], [Action::SetLocked(true)]));
        assert!(c
            .on_event(CaptureEvent::Key {
                code: LOCK,
                down: false
            })
            .is_empty());
        assert!(enter_right(&mut c).is_empty());
        c.on_event(CaptureEvent::Key {
            code: LOCK,
            down: true,
        });
        assert!(!c.locked());
        enter_right(&mut c);
        assert_eq!(c.active(), Active::Remote("lap".into()));
        let a = c.on_event(CaptureEvent::Key {
            code: LOCK,
            down: true,
        });
        assert!(
            matches!(a[..], [Action::SetLocked(true)]),
            "lock key is not forwarded"
        );
        let a = c.on_event(CaptureEvent::MotionRel { dx: -5000, dy: 0 });
        assert_eq!(a.len(), 1, "only the datagram; no Leave while locked");
        assert_eq!(c.active(), Active::Remote("lap".into()));
    }

    #[test]
    fn toggle_lock_is_the_same_state_the_hotkey_reaches() {
        // `toggle_lock()` is what the GlobalShortcuts portal calls on Wayland,
        // where no key event ever reaches `on_event`. It must be exactly the
        // state change the hotkey branch applies, or the two paths drift apart.
        let mut a = core(Side::Right, (0.0, 1.0));
        let mut b = core(Side::Right, (0.0, 1.0));

        let via_key = a.on_event(CaptureEvent::Key {
            code: LOCK,
            down: true,
        });
        let via_toggle = b.toggle_lock();
        assert_eq!(via_key, via_toggle);
        assert_eq!(a.locked(), b.locked());
        assert!(a.locked());

        // Releasing and pressing the key again is what the X11 approach would need
        // to unlock -- but on Wayland the key press is never seen, so this is the
        // only path a portal-bound toggle has to release a lock it took.
        a.on_event(CaptureEvent::Key {
            code: LOCK,
            down: false,
        });
        a.on_event(CaptureEvent::Key {
            code: LOCK,
            down: true,
        });
        b.toggle_lock();
        assert_eq!(a.locked(), b.locked());
        assert!(
            !a.locked(),
            "a lock that cannot be released is worse than no lock"
        );
    }

    #[test]
    fn disconnect_while_remote_returns_to_local() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let a = c.client_disconnected("lap");
        assert!(matches!(a[..], [Action::Ungrab { x: 960, y: 540 }]));
        assert_eq!(c.active(), Active::Local);
        assert!(enter_right(&mut c).is_empty(), "client is gone");
    }

    #[test]
    fn abort_switch_returns_to_local_without_leave() {
        let mut c = core(Side::Right, (0.0, 1.0));
        assert!(c.abort_switch().is_empty(), "nothing to abort while local");
        enter_right(&mut c);
        assert_eq!(c.active(), Active::Remote("lap".into()));
        let a = c.abort_switch();
        assert_eq!(a, vec![Action::WarpCursor { x: 960, y: 540 }]);
        assert_eq!(c.active(), Active::Local);
        // The next edge hit is a fresh crossing: the centre counts as the previous position.
        let a = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
        assert!(matches!(a[0], Action::Grab));
        assert!(has_enter(&a).is_some());
        assert_eq!(c.active(), Active::Remote("lap".into()));
    }

    #[test]
    fn leaving_a_client_ungrabs_at_the_reentry_point() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c); // virtual position (0, 250), entered at server y = 540
                             // Walk back past the client's left edge without moving vertically, so the
                             // re-entry point stays at server y = 540.
        let actions = c.on_event(CaptureEvent::MotionRel { dx: -5000, dy: 0 });
        let ungrab = actions
            .iter()
            .find_map(|a| match a {
                Action::Ungrab { x, y } => Some((*x, *y)),
                _ => None,
            })
            .expect("leaving a client must ungrab");
        // The ungrab carries the re-entry point itself, not a separate WarpCursor.
        assert_eq!(ungrab, (1918, 540));
        assert!(
            !actions
                .iter()
                .any(|a| matches!(a, Action::WarpCursor { .. })),
            "the position travels on Ungrab now: {actions:?}"
        );
    }

    #[test]
    fn sequence_numbers_increase_per_message() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let a = c.on_event(CaptureEvent::MotionRel { dx: 1, dy: 0 });
        let b = c.on_event(CaptureEvent::MotionRel { dx: 1, dy: 0 });
        let seq = |a: &Action| match a {
            Action::SendDatagram(Msg::MouseMove { seq, .. }) => *seq,
            _ => panic!(),
        };
        assert_eq!(seq(&b[0]), seq(&a[0]) + 1);
    }

    #[test]
    fn release_remote_tells_the_client_to_let_go() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        assert_eq!(c.active(), Active::Remote("lap".into()));

        let actions = c.release_remote();

        assert!(
            matches!(
                actions.first(),
                Some(Action::SendControl(Msg::Leave { .. }))
            ),
            "without Leave the client keeps whatever it is holding: {actions:?}"
        );
        assert!(
            actions.iter().any(|a| matches!(a, Action::Ungrab { .. })),
            "{actions:?}"
        );
        assert_eq!(c.active(), Active::Local);
    }

    #[test]
    fn release_remote_is_a_no_op_when_already_local() {
        let mut c = core(Side::Right, (0.0, 1.0));
        assert!(c.release_remote().is_empty());
    }

    #[test]
    fn capture_ended_returns_to_local_with_a_leave() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let actions = c.on_event(CaptureEvent::CaptureEnded);
        assert!(
            matches!(
                actions.first(),
                Some(Action::SendControl(Msg::Leave { .. }))
            ),
            "{actions:?}"
        );
        assert_eq!(c.active(), Active::Local);
    }

    #[test]
    fn capture_ended_while_local_does_nothing() {
        let mut c = core(Side::Right, (0.0, 1.0));
        assert!(c.on_event(CaptureEvent::CaptureEnded).is_empty());
    }

    #[test]
    fn a_key_held_across_release_remote_does_not_leak_into_the_next_enter() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        c.on_event(CaptureEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            down: true,
        });
        c.release_remote();
        // The key-up arrives while local, as it would from any backend that still
        // observes the keyboard.
        c.on_event(CaptureEvent::Key {
            code: KeyCode::LEFT_SHIFT,
            down: false,
        });

        c.on_event(CaptureEvent::MotionAbs { x: 1900, y: 540 });
        let actions = c.on_event(CaptureEvent::MotionAbs { x: 1919, y: 540 });
        assert_eq!(has_enter(&actions).unwrap().2, Modifiers::default());
    }

    #[test]
    fn capture_edges_covers_only_connected_clients() {
        let layout = Layout {
            server_screens: screen(1920, 1080),
            clients: vec![
                ClientPlacement {
                    name: "right".into(),
                    side: Side::Right,
                    span: (0.0, 1.0),
                },
                ClientPlacement {
                    name: "left".into(),
                    side: Side::Left,
                    span: (0.25, 0.75),
                },
            ],
        };
        let mut core = ServerCore::new(layout, Hotkeys::default());
        assert!(
            core.capture_edges().is_empty(),
            "an edge with nobody behind it would stop the pointer for nothing"
        );

        core.client_connected("left", screen(1000, 500));
        assert_eq!(core.capture_edges(), vec![(Side::Left, (0.25, 0.75))]);

        core.client_connected("right", screen(1000, 500));
        assert_eq!(
            core.capture_edges(),
            vec![(Side::Right, (0.0, 1.0)), (Side::Left, (0.25, 0.75))],
            "order follows the layout, not the connection order"
        );

        core.client_disconnected("left");
        assert_eq!(core.capture_edges(), vec![(Side::Right, (0.0, 1.0))]);
    }
}

#[cfg(test)]
mod props {
    use super::*;
    use pheme_proto::{Button, KeyCode, ScreenInfo};
    use proptest::prelude::*;

    fn ev() -> impl Strategy<Value = CaptureEvent> {
        prop_oneof![
            (0..1920i32, 0..1080i32).prop_map(|(x, y)| CaptureEvent::MotionAbs { x, y }),
            (-50..50i32, -50..50i32).prop_map(|(dx, dy)| CaptureEvent::MotionRel { dx, dy }),
            (0u16..0xE8, any::<bool>()).prop_map(|(k, d)| CaptureEvent::Key {
                code: KeyCode(k),
                down: d
            }),
            any::<bool>().prop_map(|d| CaptureEvent::Button {
                btn: Button::Left,
                down: d
            }),
        ]
    }

    proptest! {
        #[test]
        fn grab_and_ungrab_alternate(events in prop::collection::vec(ev(), 0..500)) {
            let layout = Layout {
                server_screens: vec![ScreenInfo { x: 0, y: 0, w: 1920, h: 1080, primary: true }],
                clients: vec![ClientPlacement { name: "c".into(), side: Side::Right, span: (0.0, 1.0) }],
            };
            let mut core = ServerCore::new(layout, Hotkeys { lock: Some(KeyCode(0x47)) });
            core.client_connected("c", vec![ScreenInfo { x: 0, y: 0, w: 800, h: 600, primary: true }]);
            let mut grabbed = false;
            for e in events {
                for a in core.on_event(e) {
                    match a {
                        Action::Grab => { prop_assert!(!grabbed); grabbed = true; }
                        Action::Ungrab { .. } => { prop_assert!(grabbed); grabbed = false; }
                        _ => {}
                    }
                }
                prop_assert_eq!(grabbed, core.active() != Active::Local);
            }
        }
    }
}
