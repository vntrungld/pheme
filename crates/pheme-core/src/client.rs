//! Client-side logic: applies incoming messages and guarantees every pressed key is released.

use pheme_proto::{Button, KeyCode, Msg, ScreenInfo};

use crate::geometry::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectAction {
    MoveAbs { x: i32, y: i32 },
    MoveRel { dx: i32, dy: i32 },
    Button { btn: Button, down: bool },
    Wheel { dx: i32, dy: i32 },
    Key { code: KeyCode, down: bool },
}

pub struct ClientCore {
    bounds: Rect,
    active: bool,
    /// Held keys in press order.
    held_keys: Vec<KeyCode>,
    held_buttons: Vec<Button>,
}

impl ClientCore {
    pub fn new(screens: Vec<ScreenInfo>) -> Self {
        ClientCore {
            bounds: Rect::bounds(&screens),
            active: false,
            held_keys: Vec::new(),
            held_buttons: Vec::new(),
        }
    }

    pub fn active(&self) -> bool {
        self.active
    }

    pub fn on_msg(&mut self, m: &Msg) -> Vec<InjectAction> {
        match m {
            Msg::Enter { x, y, .. } => {
                self.active = true;
                vec![InjectAction::MoveAbs {
                    x: self.bounds.x + *x as i32,
                    y: self.bounds.y + *y as i32,
                }]
            }
            Msg::Leave { .. } | Msg::Bye { .. } => self.release_all(),
            _ if !self.active => Vec::new(),
            Msg::MouseMove { dx, dy, .. } => vec![InjectAction::MoveRel {
                dx: *dx as i32,
                dy: *dy as i32,
            }],
            Msg::MouseAbs { x, y, .. } => {
                vec![InjectAction::MoveAbs {
                    x: self.bounds.x + *x as i32,
                    y: self.bounds.y + *y as i32,
                }]
            }
            Msg::Wheel { dx, dy, .. } => vec![InjectAction::Wheel {
                dx: *dx as i32,
                dy: *dy as i32,
            }],
            Msg::Button { btn, down, .. } => {
                if *down {
                    if !self.held_buttons.contains(btn) {
                        self.held_buttons.push(*btn);
                    }
                } else {
                    self.held_buttons.retain(|b| b != btn);
                }
                vec![InjectAction::Button {
                    btn: *btn,
                    down: *down,
                }]
            }
            Msg::Key { code, down, .. } => {
                if *down {
                    if !self.held_keys.contains(code) {
                        self.held_keys.push(*code);
                    }
                } else {
                    self.held_keys.retain(|k| k != code);
                }
                vec![InjectAction::Key {
                    code: *code,
                    down: *down,
                }]
            }
            _ => Vec::new(),
        }
    }

    pub fn on_disconnect(&mut self) -> Vec<InjectAction> {
        self.release_all()
    }

    fn release_all(&mut self) -> Vec<InjectAction> {
        self.active = false;
        let mut out = Vec::new();
        for b in self.held_buttons.drain(..) {
            out.push(InjectAction::Button {
                btn: b,
                down: false,
            });
        }
        let (mods, keys): (Vec<KeyCode>, Vec<KeyCode>) =
            self.held_keys.drain(..).partition(|k| k.is_modifier());
        for k in keys.into_iter().chain(mods) {
            out.push(InjectAction::Key {
                code: k,
                down: false,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_proto::{Button, KeyCode, Modifiers, Msg, ScreenInfo};

    fn core() -> ClientCore {
        ClientCore::new(vec![
            ScreenInfo {
                x: -1000,
                y: 0,
                w: 1000,
                h: 500,
                primary: false,
            },
            ScreenInfo {
                x: 0,
                y: 0,
                w: 800,
                h: 600,
                primary: true,
            },
        ])
    }

    #[test]
    fn enter_moves_to_absolute_position_offset_by_bounds() {
        let mut c = core();
        let a = c.on_msg(&Msg::Enter {
            seq: 1,
            x: 10,
            y: 20,
            mods: Modifiers(0),
        });
        assert_eq!(a, vec![InjectAction::MoveAbs { x: -990, y: 20 }]);
        assert!(c.active());
    }

    #[test]
    fn motion_and_wheel_pass_through() {
        let mut c = core();
        c.on_msg(&Msg::Enter {
            seq: 1,
            x: 0,
            y: 0,
            mods: Modifiers(0),
        });
        assert_eq!(
            c.on_msg(&Msg::MouseMove {
                seq: 2,
                dx: 3,
                dy: -4
            }),
            vec![InjectAction::MoveRel { dx: 3, dy: -4 }]
        );
        assert_eq!(
            c.on_msg(&Msg::Wheel {
                seq: 3,
                dx: 0,
                dy: 120
            }),
            vec![InjectAction::Wheel { dx: 0, dy: 120 }]
        );
        assert_eq!(
            c.on_msg(&Msg::MouseAbs { seq: 4, x: 5, y: 6 }),
            vec![InjectAction::MoveAbs { x: -995, y: 6 }]
        );
    }

    #[test]
    fn input_is_ignored_before_enter_and_after_leave() {
        let mut c = core();
        assert!(c
            .on_msg(&Msg::MouseMove {
                seq: 1,
                dx: 1,
                dy: 1
            })
            .is_empty());
        c.on_msg(&Msg::Enter {
            seq: 1,
            x: 0,
            y: 0,
            mods: Modifiers(0),
        });
        c.on_msg(&Msg::Leave { seq: 2 });
        assert!(!c.active());
        assert!(c
            .on_msg(&Msg::Key {
                seq: 3,
                code: KeyCode(0x04),
                down: true
            })
            .is_empty());
    }

    #[test]
    fn leave_releases_buttons_then_keys_then_modifiers() {
        let mut c = core();
        c.on_msg(&Msg::Enter {
            seq: 1,
            x: 0,
            y: 0,
            mods: Modifiers(0),
        });
        c.on_msg(&Msg::Key {
            seq: 2,
            code: KeyCode::LEFT_CTRL,
            down: true,
        });
        c.on_msg(&Msg::Key {
            seq: 3,
            code: KeyCode(0x04),
            down: true,
        });
        c.on_msg(&Msg::Key {
            seq: 4,
            code: KeyCode(0x05),
            down: true,
        });
        c.on_msg(&Msg::Button {
            seq: 5,
            btn: Button::Left,
            down: true,
        });
        c.on_msg(&Msg::Key {
            seq: 6,
            code: KeyCode(0x05),
            down: false,
        });
        let a = c.on_msg(&Msg::Leave { seq: 7 });
        assert_eq!(
            a,
            vec![
                InjectAction::Button {
                    btn: Button::Left,
                    down: false
                },
                InjectAction::Key {
                    code: KeyCode(0x04),
                    down: false
                },
                InjectAction::Key {
                    code: KeyCode::LEFT_CTRL,
                    down: false
                },
            ]
        );
        assert!(
            c.on_msg(&Msg::Leave { seq: 8 }).is_empty(),
            "nothing left to release"
        );
    }

    #[test]
    fn disconnect_and_bye_release_everything() {
        let mut c = core();
        c.on_msg(&Msg::Enter {
            seq: 1,
            x: 0,
            y: 0,
            mods: Modifiers(0),
        });
        c.on_msg(&Msg::Key {
            seq: 2,
            code: KeyCode(0x04),
            down: true,
        });
        assert_eq!(
            c.on_disconnect(),
            vec![InjectAction::Key {
                code: KeyCode(0x04),
                down: false
            }]
        );
        c.on_msg(&Msg::Enter {
            seq: 3,
            x: 0,
            y: 0,
            mods: Modifiers(0),
        });
        c.on_msg(&Msg::Button {
            seq: 4,
            btn: Button::Right,
            down: true,
        });
        assert_eq!(
            c.on_msg(&Msg::Bye { reason: "x".into() }),
            vec![InjectAction::Button {
                btn: Button::Right,
                down: false
            }]
        );
    }
}
