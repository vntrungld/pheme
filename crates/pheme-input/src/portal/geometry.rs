//! Turning the core's capture edges into portal pointer barriers.
//!
//! Two rules from the portal specification drive everything here, and both fail
//! silently when broken — the compositor reports a rejected barrier only in
//! `failed_barriers`, while `SetPointerBarriers` and `Enable` both still succeed:
//!
//! 1. A barrier sits on the top (horizontal) or left (vertical) edge of its pixels,
//!    so the far boundary of a zone of width `W` at `x0` is `x0 + W`, while the
//!    extent along the edge stops at the last pixel, `x0 + W - 1`.
//! 2. A barrier must lie on the outside boundary of the union of all zones **and**
//!    be fully contained within a single zone.
//! 3. A barrier must span its zone's **whole** edge. One pixel short is rejected.

use std::num::NonZeroU32;

use pheme_core::Side;

use crate::CaptureEdge;

/// One of the compositor's input zones, as reported by `GetZones`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Zone {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

impl Zone {
    fn x1(&self) -> i32 {
        self.x + self.w as i32
    }
    fn y1(&self) -> i32 {
        self.y + self.h as i32
    }
}

/// A barrier ready to hand to `SetPointerBarriers`, with the side it came from so an
/// `Activated` signal can be traced back to a screen edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalBarrier {
    pub id: NonZeroU32,
    pub side: Side,
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

fn union(zones: &[Zone]) -> Option<(i32, i32, i32, i32)> {
    let x0 = zones.iter().map(|z| z.x).min()?;
    let y0 = zones.iter().map(|z| z.y).min()?;
    let x1 = zones.iter().map(Zone::x1).max()?;
    let y1 = zones.iter().map(Zone::y1).max()?;
    Some((x0, y0, x1, y1))
}

/// The bounding box of the compositor's zones, in the same form the core uses for the
/// server's screens — `None` when there are no zones at all.
///
/// This is what a barrier is placed against, so it is also what has to agree with the
/// rect the core clamps and tests edges against. `ZonesChanged` is the moment the two
/// can silently stop agreeing (§9).
pub fn zone_bounds(zones: &[Zone]) -> Option<pheme_core::Rect> {
    let (x0, y0, x1, y1) = union(zones)?;
    Some(pheme_core::Rect {
        x: x0,
        y: y0,
        w: x1 - x0,
        h: y1 - y0,
    })
}

/// Maps capture edges onto portal barriers, splitting each edge across the zones that
/// touch it. Returns barriers with ids starting at 1; the order is stable so tests can
/// name positions.
///
/// The "union" computed here is deliberately the same bounding box
/// `pheme_core::Rect::bounds` gives the core, not the true union of the zones' pixels.
/// On a staggered layout — a 1920x1080 laptop at (0, 0) beside a 2560x1440 monitor at
/// (1920, 0) — the laptop's own bottom edge at y = 1079 lies *inside* that bounding box,
/// so it gets no barrier here, and that is intentional: `pheme_core`'s `on_edge` also
/// recognises switch edges only against the bounding box. A barrier at y = 1079 would
/// let the compositor capture the pointer at a spot the core does not treat as an edge,
/// so the crossing would go nowhere — the pointer would simply stop working there. Spans
/// are resolved against this same bounding box for the same reason (`edge_segment`
/// resolves them the same way), keeping the two consistent by construction.
///
/// A span selects the zones that take part; it never narrows a barrier within one,
/// because the compositor rejects a barrier that does not span its zone's whole edge.
/// The core still applies the span exactly — see the comment on the widening below. Widening
/// this to the true per-zone union needs `pheme_core`'s geometry widened first, since
/// both `Rect::bounds` and `on_edge` would need it, and the X11 backend has the
/// identical bounding-box limitation today.
pub fn barriers(zones: &[Zone], edges: &[CaptureEdge]) -> Vec<PortalBarrier> {
    let Some((ux0, uy0, ux1, uy1)) = union(zones) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut next_id = NonZeroU32::MIN;
    for e in edges {
        let vertical = matches!(e.side, Side::Left | Side::Right);
        // The fixed coordinate of this edge, and the span of the union along it.
        let (fixed, u_lo, u_hi) = match e.side {
            Side::Left => (ux0, uy0, uy1),
            Side::Right => (ux1, uy0, uy1),
            Side::Top => (uy0, ux0, ux1),
            Side::Bottom => (uy1, ux0, ux1),
        };
        let len = (u_hi - u_lo) as f32;
        let a = e.span.0.clamp(0.0, 1.0);
        let b = e.span.1.clamp(0.0, 1.0);
        // Half-open along the edge, exactly like `EdgeSegment`.
        let want_lo = u_lo + (a * len).round() as i32;
        let want_hi = u_lo + (b * len).round() as i32;
        for z in zones {
            // Rule 2: only a zone whose own boundary is the union's boundary here.
            let on_this_edge = match e.side {
                Side::Left => z.x == ux0,
                Side::Right => z.x1() == ux1,
                Side::Top => z.y == uy0,
                Side::Bottom => z.y1() == uy1,
            };
            if !on_this_edge {
                continue;
            }
            let (z_lo, z_hi) = if vertical {
                (z.y, z.y1())
            } else {
                (z.x, z.x1())
            };
            // The span decides *which* zones take part, not how much of one they cover.
            if want_hi.min(z_hi) <= want_lo.max(z_lo) {
                continue;
            }
            // Rule 3: the barrier covers this zone's whole edge. Measured against
            // KWin with this function's own output: span (0.0, 1.0) on a 2560x1440
            // zone gives (2560,0)-(2560,1439) and is accepted, while (0.25, 0.75)
            // gives (2560,360)-(2560,1079), (0.0, 0.5) gives (2560,0)-(2560,719) and
            // (0.5, 1.0) gives (2560,720)-(2560,1439) -- every one of them rejected,
            // as is a barrier one pixel short of the full edge. A narrowed barrier is
            // therefore not a narrower crossing region, it is no crossing region at
            // all, and `set_edges` turns the rejection into a startup error.
            //
            // Nothing is lost by widening it. An activation outside the configured
            // span arrives as `CaptureEvent::CaptureActivated`, `ServerCore` finds no
            // placement whose `EdgeSegment` contains it, and answers `Action::Ungrab`
            // one pixel inside the edge -- the same path that already handles a
            // crossing declined because the pointer is locked. The span stays exact;
            // only the barrier is coarse.
            let (lo, hi) = (z_lo, z_hi);
            let id = next_id;
            next_id = next_id
                .checked_add(1)
                .expect("fewer than u32::MAX barriers are ever produced");
            // `hi` is exclusive; the barrier's far end is the last pixel (rule 1).
            out.push(if vertical {
                PortalBarrier {
                    id,
                    side: e.side,
                    x1: fixed,
                    y1: lo,
                    x2: fixed,
                    y2: hi - 1,
                }
            } else {
                PortalBarrier {
                    id,
                    side: e.side,
                    x1: lo,
                    y1: fixed,
                    x2: hi - 1,
                    y2: fixed,
                }
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_core::Side;

    fn one_screen() -> Vec<Zone> {
        vec![Zone {
            x: 0,
            y: 0,
            w: 2560,
            h: 1440,
        }]
    }

    /// Two 1920x1080 monitors side by side, as the portal specification's own example.
    fn two_screens() -> Vec<Zone> {
        vec![
            Zone {
                x: 0,
                y: 0,
                w: 1920,
                h: 1080,
            },
            Zone {
                x: 1920,
                y: 0,
                w: 1920,
                h: 1080,
            },
        ]
    }

    fn edge(side: Side) -> CaptureEdge {
        CaptureEdge {
            side,
            span: (0.0, 1.0),
        }
    }

    #[test]
    fn the_far_edge_is_the_width_not_the_last_pixel() {
        let b = barriers(&one_screen(), &[edge(Side::Right)]);
        assert_eq!(b.len(), 1);
        // Measured: a barrier at x = 2559 is rejected by the compositor, and the
        // rejection is reported only in failed_barriers -- Enable still succeeds and
        // the session then never activates.
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (2560, 0, 2560, 1439));
    }

    #[test]
    fn a_horizontal_edge_stops_at_the_last_pixel_along_it() {
        let b = barriers(&one_screen(), &[edge(Side::Bottom)]);
        assert_eq!(b.len(), 1);
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (0, 1440, 2559, 1440));
    }

    #[test]
    fn the_near_edges_sit_at_the_origin() {
        let l = barriers(&one_screen(), &[edge(Side::Left)]);
        assert_eq!(l.len(), 1);
        assert_eq!((l[0].x1, l[0].y1, l[0].x2, l[0].y2), (0, 0, 0, 1439));
        let t = barriers(&one_screen(), &[edge(Side::Top)]);
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].x1, t[0].y1, t[0].x2, t[0].y2), (0, 0, 2559, 0));
    }

    #[test]
    fn a_union_edge_spanning_two_zones_is_split_per_zone() {
        // The portal requires a barrier to be fully contained within one zone. The top
        // edge of this union crosses both monitors, so one barrier across it is
        // rejected. This is invisible on a single-monitor machine.
        let b = barriers(&two_screens(), &[edge(Side::Top)]);
        assert_eq!(b.len(), 2, "{b:?}");
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (0, 0, 1919, 0));
        assert_eq!((b[1].x1, b[1].y1, b[1].x2, b[1].y2), (1920, 0, 3839, 0));
    }

    #[test]
    fn only_zones_touching_that_side_of_the_union_get_a_barrier() {
        // The right edge of this union belongs to the right monitor alone.
        let b = barriers(&two_screens(), &[edge(Side::Right)]);
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (3840, 0, 3840, 1079));
    }

    #[test]
    fn a_span_never_narrows_the_barrier_within_a_zone() {
        // Measured: (2560,360)-(2560,1079) -- the narrowed barrier this function used
        // to produce for this very span -- is rejected by the compositor, as is every
        // barrier short of the zone's full edge. The span is applied by the core when
        // the activation arrives, not by the barrier.
        for span in [(0.25, 0.75), (0.0, 0.5), (0.5, 1.0)] {
            let e = CaptureEdge {
                side: Side::Right,
                span,
            };
            let b = barriers(&one_screen(), &[e]);
            assert_eq!(b.len(), 1, "span {span:?}: {b:?}");
            assert_eq!(
                (b[0].x1, b[0].y1, b[0].x2, b[0].y2),
                (2560, 0, 2560, 1439),
                "span {span:?} must still cover the whole edge"
            );
        }
    }

    #[test]
    fn a_span_still_chooses_which_zones_take_part() {
        // Zone granularity is the one narrowing the compositor does accept: the left
        // half of this union is the left monitor, so only it gets a barrier -- and
        // that barrier covers the whole of its own edge.
        let e = CaptureEdge {
            side: Side::Top,
            span: (0.0, 0.5),
        };
        let b = barriers(&two_screens(), &[e]);
        assert_eq!(b.len(), 1, "{b:?}");
        assert_eq!((b[0].x1, b[0].y1, b[0].x2, b[0].y2), (0, 0, 1919, 0));
    }

    #[test]
    fn ids_are_unique_and_map_back_to_their_side() {
        let b = barriers(&two_screens(), &[edge(Side::Top), edge(Side::Right)]);
        let ids: std::collections::BTreeSet<u32> = b.iter().map(|b| b.id.get()).collect();
        assert_eq!(ids.len(), b.len(), "ids must be unique: {b:?}");
        assert!(
            b.iter().all(|b| b.id.get() != 0),
            "zero is not a valid barrier id"
        );
        let top = b.iter().filter(|b| b.side == Side::Top).count();
        assert_eq!(top, 2);
    }

    #[test]
    fn no_edges_means_no_barriers() {
        assert!(barriers(&one_screen(), &[]).is_empty());
    }

    #[test]
    fn a_degenerate_span_produces_nothing_rather_than_an_invalid_barrier() {
        let e = CaptureEdge {
            side: Side::Right,
            span: (0.5, 0.5),
        };
        assert!(barriers(&one_screen(), &[e]).is_empty());
    }
}
