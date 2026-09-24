//! The portal session thread: D-Bus on one side, libei on the other, and the
//! `CaptureEvent` stream `ServerCore` consumes coming out.
//!
//! Task 8 wires `establish`, `Session`'s methods, `bind_seat` and `Cmd` into its
//! dispatch loop. Until then this module has no caller, so it is allowed to be
//! dead code; Task 8's dispatch removes this allow along with the last unused item.
#![allow(dead_code)]

use std::os::unix::net::UnixStream;

use ashpd::desktop::input_capture::{
    Barrier, BarrierPosition, Capabilities, ConnectToEISOptions, CreateSession2Options,
    DisableOptions, EnableOptions, GetZonesOptions, InputCapture as Portal,
    SetPointerBarriersOptions, StartOptions,
};
use ashpd::desktop::PersistMode;
use reis::ei;
use reis::event::DeviceCapability;
use tracing::{debug, error, info, warn};

use crate::portal::geometry::{barriers, Zone};
use crate::{CaptureEdge, Error, Result};

// `ReleaseOptions`, `crossbeam_channel::Sender`, `futures_lite::StreamExt`,
// `pheme_core::{CaptureEvent, Rect}`, `reis::event::EiEvent` and
// `crate::portal::translate::*` are needed by Task 8's dispatch loop, not by
// anything this module defines yet; that task imports them itself.

/// Commands the backend thread accepts. Each carries an acknowledgement channel, so
/// the trait's synchronous contract is met by waiting for the thread to answer.
pub(crate) enum Cmd {
    SetEdges(Vec<CaptureEdge>, async_channel::Sender<Result<()>>),
    Release {
        x: i32,
        y: i32,
        ack: async_channel::Sender<Result<()>>,
    },
    Stop,
}

struct Session {
    // `ashpd::desktop::Session<T>` takes one generic parameter and no lifetime, and
    // `InputCapture` has no lifetime parameter either.
    portal: Portal,
    session: ashpd::desktop::Session<Portal>,
    zones: Vec<Zone>,
    zone_set: u32,
    /// The barrier ids currently declared, and which side each came from.
    sides: std::collections::HashMap<u32, pheme_core::Side>,
    /// Set while a capture is running; `Release` is ignored for any other id.
    activation: Option<u32>,
}

async fn establish(edges: &[CaptureEdge]) -> Result<(Session, ei::Context)> {
    let portal = Portal::new().await.map_err(pe)?;
    let session = portal
        .create_session2(CreateSession2Options::default())
        .await
        .map_err(pe)?;

    let start = portal
        .start(
            &session,
            None,
            StartOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(pe)?
        .response()
        .map_err(pe)?;

    // Read what was GRANTED, not what was asked for. KDE advertises Touchscreen and
    // does not grant it; a backend that assumes the request was honoured would wait
    // for devices that never appear.
    let granted = start.capabilities();
    if !granted.contains(Capabilities::Pointer) {
        return Err(Error::Permission(
            "the compositor did not grant pointer capture".into(),
        ));
    }
    if !granted.contains(Capabilities::Keyboard) {
        warn!("the compositor granted pointer capture but not keyboard capture");
    }
    info!(?granted, restore_token = ?start.restore_token(), "input capture session started");

    let mut s = Session {
        portal,
        session,
        zones: Vec::new(),
        zone_set: 0,
        sides: Default::default(),
        activation: None,
    };
    s.refresh_zones().await?;
    s.set_barriers(edges).await?;

    let fd = s
        .portal
        .connect_to_eis(&s.session, ConnectToEISOptions::default())
        .await
        .map_err(pe)?;
    let stream = UnixStream::from(fd);
    stream
        .set_nonblocking(true)
        .map_err(|e| Error::Backend(e.to_string()))?;
    let ctx = ei::Context::new(stream).map_err(|e| Error::Backend(e.to_string()))?;
    Ok((s, ctx))
}

fn pe(e: impl std::fmt::Display) -> Error {
    Error::Backend(format!("portal: {e}"))
}

impl Session {
    async fn refresh_zones(&mut self) -> Result<()> {
        let z = self
            .portal
            .zones(&self.session, GetZonesOptions::default())
            .await
            .map_err(pe)?
            .response()
            .map_err(pe)?;
        self.zones = z
            .regions()
            .iter()
            .map(|r| Zone {
                x: r.x_offset(),
                y: r.y_offset(),
                w: r.width(),
                h: r.height(),
            })
            .collect();
        self.zone_set = z.zone_set();
        debug!(zone_set = self.zone_set, zones = ?self.zones, "zones");
        Ok(())
    }

    /// Declares barriers for `edges` and re-enables the session.
    ///
    /// `SetPointerBarriers` suspends the session, so `Enable` must follow *every*
    /// call — not only the first. A client connecting mid-session lands here.
    async fn set_barriers(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        let want = barriers(&self.zones, edges);
        self.sides = want.iter().map(|b| (b.id.get(), b.side)).collect();
        let list: Vec<Barrier> = want
            .iter()
            .map(|b| Barrier::new(b.id, BarrierPosition::new(b.x1, b.y1, b.x2, b.y2)))
            .collect();
        let resp = self
            .portal
            .set_pointer_barriers(
                &self.session,
                &list,
                self.zone_set,
                SetPointerBarriersOptions::default(),
            )
            .await
            .map_err(pe)?
            .response()
            .map_err(pe)?;

        // A rejected barrier is reported ONLY here: the call succeeds, Enable
        // succeeds, and the session then never activates. Never swallow this.
        let failed = resp.failed_barriers();
        if !failed.is_empty() {
            for id in failed {
                let side = self.sides.get(&id.get());
                error!(
                    id = id.get(),
                    ?side,
                    "the compositor rejected this pointer barrier"
                );
            }
            return Err(Error::Backend(format!(
                "the compositor rejected {} of {} pointer barriers",
                failed.len(),
                list.len()
            )));
        }

        if list.is_empty() {
            // No edges left: stop capturing entirely rather than leave a session
            // armed with nothing to trigger it.
            self.portal
                .disable(&self.session, DisableOptions::default())
                .await
                .map_err(pe)?;
            return Ok(());
        }
        self.portal
            .enable(&self.session, EnableOptions::default())
            .await
            .map_err(pe)
    }
}

/// Binds the capabilities we need on every seat the compositor offers.
///
/// The `flush` is load-bearing: without it the bind request never leaves the
/// buffer, the compositor creates no devices, and not one input event ever
/// arrives — with no error and nothing in any log. This was reproduced during
/// design: `SeatAdded` followed by silence.
fn bind_seat(ctx: &ei::Context, seat: &reis::event::Seat) {
    seat.bind_capabilities(
        DeviceCapability::Pointer
            | DeviceCapability::Keyboard
            | DeviceCapability::Scroll
            | DeviceCapability::Button,
    );
    if let Err(e) = ctx.flush() {
        error!("flushing the libei capability bind failed: {e}; no devices will appear");
    }
}
