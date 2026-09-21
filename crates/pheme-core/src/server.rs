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
    MotionAbs { x: i32, y: i32 },
    MotionRel { dx: i32, dy: i32 },
    Button { btn: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    SendControl(Msg),
    SendDatagram(Msg),
    Grab,
    Ungrab,
    WarpCursor { x: i32, y: i32 },
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

    pub fn client_connected(&mut self, name: &str, screens: Vec<ScreenInfo>) -> Vec<Action> {
        debug!(name, ?screens, "client connected");
        self.connected
            .insert(name.to_string(), Rect::bounds(&screens));
        Vec::new()
    }

    pub fn client_disconnected(&mut self, name: &str) -> Vec<Action> {
        self.connected.remove(name);
        match &self.remote {
            Some(r) if r.name == name => {
                self.remote = None;
                let (x, y) = self.server.center();
                self.last_pos = Some((x, y));
                vec![Action::Ungrab, Action::WarpCursor { x, y }]
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

    pub fn on_event(&mut self, ev: CaptureEvent) -> Vec<Action> {
        if let CaptureEvent::Key { code, down } = ev {
            if Some(code) == self.hotkeys.lock {
                if down {
                    self.locked = !self.locked;
                    return vec![Action::SetLocked(self.locked)];
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
                    actions.push(Action::Ungrab);
                    actions.push(Action::WarpCursor { x, y });
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
        assert!(matches!(a[1], Action::Ungrab));
        assert!(matches!(a[2], Action::WarpCursor { x: 1918, y: 0 }));
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
    fn disconnect_while_remote_returns_to_local() {
        let mut c = core(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let a = c.client_disconnected("lap");
        assert!(matches!(
            a[..],
            [Action::Ungrab, Action::WarpCursor { x: 960, y: 540 }]
        ));
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
                        Action::Ungrab => { prop_assert!(grabbed); grabbed = false; }
                        _ => {}
                    }
                }
                prop_assert_eq!(grabbed, core.active() != Active::Local);
            }
        }
    }
}
