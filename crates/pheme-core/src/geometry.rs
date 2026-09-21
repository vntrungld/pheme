//! Screen-edge geometry: bounding rectangles, edge segments and projections.

use pheme_proto::ScreenInfo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Left,
    Right,
    Top,
    Bottom,
}

impl Side {
    pub fn opposite(self) -> Side {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
            Side::Top => Side::Bottom,
            Side::Bottom => Side::Top,
        }
    }

    fn is_vertical(self) -> bool {
        matches!(self, Side::Left | Side::Right)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl Rect {
    /// Bounding rectangle of all screens; always at least 1×1, never degenerate.
    pub fn bounds(screens: &[ScreenInfo]) -> Rect {
        let mut it = screens.iter();
        let Some(first) = it.next() else {
            return Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            };
        };
        let (mut x0, mut y0) = (first.x, first.y);
        let (mut x1, mut y1) = (first.x + first.w as i32, first.y + first.h as i32);
        for s in it {
            x0 = x0.min(s.x);
            y0 = y0.min(s.y);
            x1 = x1.max(s.x + s.w as i32);
            y1 = y1.max(s.y + s.h as i32);
        }
        Rect {
            x: x0,
            y: y0,
            w: (x1 - x0).max(1),
            h: (y1 - y0).max(1),
        }
    }

    pub fn center(&self) -> (i32, i32) {
        (self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// A range `[start, end)` along one side of a rect, in pixels relative to the rect origin
/// (y for Left/Right, x for Top/Bottom).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EdgeSegment {
    pub side: Side,
    pub start: i32,
    pub end: i32,
}

impl EdgeSegment {
    fn len(&self) -> i32 {
        (self.end - self.start).max(1)
    }

    pub fn contains(&self, along: i32) -> bool {
        along >= self.start && along < self.end
    }
}

pub fn edge_segment(rect: &Rect, side: Side, span: (f32, f32)) -> EdgeSegment {
    let len = if side.is_vertical() { rect.h } else { rect.w } as f32;
    let (a, b) = (span.0.clamp(0.0, 1.0), span.1.clamp(0.0, 1.0));
    EdgeSegment {
        side,
        start: (a * len).round() as i32,
        end: (b * len).round() as i32,
    }
}

/// If `(x, y)` lies exactly on `side` of `rect`, returns the along-edge coordinate relative to the rect origin.
pub fn on_edge(rect: &Rect, side: Side, x: i32, y: i32) -> Option<i32> {
    let hit = match side {
        Side::Left => x == rect.x,
        Side::Right => x == rect.x + rect.w - 1,
        Side::Top => y == rect.y,
        Side::Bottom => y == rect.y + rect.h - 1,
    };
    if !hit || !rect.contains(x, y) {
        return None;
    }
    Some(if side.is_vertical() {
        y - rect.y
    } else {
        x - rect.x
    })
}

/// Maps an along-edge coordinate on the server segment to the entry point on the client's
/// opposite edge, relative to the client rect origin.
pub fn project_entry(seg: &EdgeSegment, along: i32, client: &Rect) -> (u16, u16) {
    let t = (along - seg.start) as f32 / seg.len() as f32;
    let (x, y) = match seg.side {
        Side::Right => (0, (t * client.h as f32) as i32),
        Side::Left => (client.w - 1, (t * client.h as f32) as i32),
        Side::Bottom => ((t * client.w as f32) as i32, 0),
        Side::Top => ((t * client.w as f32) as i32, client.h - 1),
    };
    (
        x.clamp(0, client.w - 1) as u16,
        y.clamp(0, client.h - 1) as u16,
    )
}

/// Maps the virtual client position `(vx, vy)` (relative to the client origin, possibly
/// just outside it) back to an absolute server point one pixel inside the server edge.
pub fn project_exit(
    seg: &EdgeSegment,
    client: &Rect,
    vx: i32,
    vy: i32,
    server: &Rect,
) -> (i32, i32) {
    let t = match seg.side {
        Side::Left | Side::Right => vy.clamp(0, client.h - 1) as f32 / client.h as f32,
        Side::Top | Side::Bottom => vx.clamp(0, client.w - 1) as f32 / client.w as f32,
    };
    let along = seg.start + (t * seg.len() as f32) as i32;
    match seg.side {
        Side::Right => (server.x + server.w - 2, server.y + along),
        Side::Left => (server.x + 1, server.y + along),
        Side::Bottom => (server.x + along, server.y + server.h - 2),
        Side::Top => (server.x + along, server.y + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_proto::ScreenInfo;

    fn s(x: i32, y: i32, w: u32, h: u32) -> ScreenInfo {
        ScreenInfo {
            x,
            y,
            w,
            h,
            primary: false,
        }
    }

    #[test]
    fn bounds_of_two_monitors() {
        let r = Rect::bounds(&[s(0, 0, 1920, 1080), s(1920, -200, 2560, 1440)]);
        assert_eq!(
            r,
            Rect {
                x: 0,
                y: -200,
                w: 4480,
                h: 1440
            }
        );
        assert_eq!(r.center(), (2240, 520));
    }

    #[test]
    fn bounds_of_nothing_is_a_unit_rect() {
        assert_eq!(
            Rect::bounds(&[]),
            Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1
            }
        );
    }

    #[test]
    fn bounds_never_degenerate_to_zero_size() {
        let r = Rect::bounds(&[s(10, 20, 0, 0)]);
        assert_eq!(
            r,
            Rect {
                x: 10,
                y: 20,
                w: 1,
                h: 1
            }
        );
        // projections must not panic on the smallest possible client rect
        let seg = EdgeSegment {
            side: Side::Right,
            start: 0,
            end: 1080,
        };
        assert_eq!(project_entry(&seg, 540, &r), (0, 0));
        let server = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(project_exit(&seg, &r, -1, 0, &server), (1918, 0));
    }

    #[test]
    fn edge_segment_full_and_partial() {
        let r = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        assert_eq!(
            edge_segment(&r, Side::Right, (0.0, 1.0)),
            EdgeSegment {
                side: Side::Right,
                start: 0,
                end: 1080
            }
        );
        assert_eq!(
            edge_segment(&r, Side::Top, (0.25, 0.5)),
            EdgeSegment {
                side: Side::Top,
                start: 480,
                end: 960
            }
        );
    }

    #[test]
    fn on_edge_detects_each_side() {
        let r = Rect {
            x: 100,
            y: 50,
            w: 1920,
            h: 1080,
        };
        assert_eq!(on_edge(&r, Side::Left, 100, 300), Some(250));
        assert_eq!(on_edge(&r, Side::Right, 2019, 300), Some(250));
        assert_eq!(on_edge(&r, Side::Top, 700, 50), Some(600));
        assert_eq!(on_edge(&r, Side::Bottom, 700, 1129), Some(600));
        assert_eq!(on_edge(&r, Side::Right, 2018, 300), None);
        assert_eq!(on_edge(&r, Side::Left, 101, 300), None);
    }

    #[test]
    fn entry_projection_scales_to_client_size() {
        let seg = EdgeSegment {
            side: Side::Right,
            start: 0,
            end: 1000,
        };
        let client = Rect {
            x: 0,
            y: 0,
            w: 500,
            h: 2000,
        };
        assert_eq!(project_entry(&seg, 500, &client), (0, 1000));
        let seg = EdgeSegment {
            side: Side::Left,
            start: 0,
            end: 1000,
        };
        assert_eq!(project_entry(&seg, 0, &client), (499, 0));
        let seg = EdgeSegment {
            side: Side::Bottom,
            start: 200,
            end: 400,
        };
        assert_eq!(project_entry(&seg, 300, &client), (250, 0));
        let seg = EdgeSegment {
            side: Side::Top,
            start: 0,
            end: 100,
        };
        assert_eq!(project_entry(&seg, 99, &client), (495, 1999));
    }

    #[test]
    fn exit_projection_lands_one_pixel_inside_server_edge() {
        let server = Rect {
            x: 0,
            y: 0,
            w: 1920,
            h: 1080,
        };
        let client = Rect {
            x: 0,
            y: 0,
            w: 1000,
            h: 500,
        };
        let seg = EdgeSegment {
            side: Side::Right,
            start: 0,
            end: 1080,
        };
        // leaving the client through its left edge at vy = 250 (middle) → server right edge, middle
        assert_eq!(project_exit(&seg, &client, -1, 250, &server), (1918, 540));
        let seg = EdgeSegment {
            side: Side::Top,
            start: 0,
            end: 1920,
        };
        assert_eq!(project_exit(&seg, &client, 500, 500, &server), (960, 1));
    }
}
