//! The portal session thread: D-Bus on one side, libei on the other, and the
//! `CaptureEvent` stream `ServerCore` consumes coming out.

use std::os::unix::net::UnixStream;
use std::rc::Rc;

use ashpd::desktop::input_capture::{
    Activated, ActivatedBarrier, Barrier, BarrierPosition, Capabilities, ConnectToEISOptions,
    CreateSession2Options, Deactivated, DisableOptions, Disabled, EnableOptions, GetZonesOptions,
    InputCapture as Portal, ReleaseOptions, SetPointerBarriersOptions, StartOptions, ZonesChanged,
};
use ashpd::desktop::PersistMode;
use crossbeam_channel::Sender as EventSender;
use futures_lite::stream::{Stream, StreamExt};
use pheme_core::{CaptureEvent, Rect};
use reis::ei;
use reis::event::{DeviceCapability, EiEvent};
use tracing::{debug, error, info, warn};

use crate::portal::geometry::{barriers, Zone};
use crate::portal::translate::{
    button_from_evdev, clamp_into, key_from_evdev, modifier_keys, wheel_from_discrete, HeldKeys,
    Motion,
};
use crate::{CaptureEdge, Error, Result};

/// Commands the backend thread accepts. Each carries an acknowledgement channel, so
/// the trait's synchronous contract is met by waiting for the thread to answer.
///
/// The acknowledgement travels on `std::sync::mpsc`, not `async_channel`: the
/// trait's contract needs a *timed* receive, which `std::sync::mpsc::Receiver`
/// has natively (`recv_timeout`) and `async_channel` 2.x does not expose. The
/// command direction stays on `async_channel` because the loop below awaits it
/// inside the executor alongside the portal and libei streams.
pub(crate) enum Cmd {
    SetEdges(Vec<CaptureEdge>, std::sync::mpsc::Sender<Result<()>>),
    Release {
        x: i32,
        y: i32,
        ack: std::sync::mpsc::Sender<Result<()>>,
    },
    Stop,
}

struct Session {
    // `ashpd::desktop::Session<T>` takes one generic parameter and no lifetime, and
    // `InputCapture` has no lifetime parameter either.
    //
    // Shared rather than owned outright because `run` has to subscribe to the
    // session's signals through a handle of its own: ashpd's `receive_*` return an
    // `impl Stream` that captures `&self`, so a stream created from this field
    // would borrow the whole `Session` for as long as it lives and nothing could
    // then call `set_barriers`, which needs `&mut Session`. `Rc` keeps the two
    // borrows apart; the session never leaves its thread, so it needs no `Arc`.
    portal: Rc<Portal>,
    session: ashpd::desktop::Session<Portal>,
    zones: Vec<Zone>,
    zone_set: u32,
    /// The barrier ids currently declared, and which side each came from.
    sides: std::collections::HashMap<u32, pheme_core::Side>,
    /// Set while a capture is running; `Release` is ignored for any other id.
    activation: Option<u32>,
    /// True once `Enable` has succeeded and the barriers it armed have not been taken
    /// away again. It is what tells an empty barrier set that has something to remove
    /// from one that has not — `start()` always runs before any client can connect, so
    /// the first `set_barriers` is always the empty one.
    armed: bool,
}

/// Creates the session, starts it, fetches the zones and connects to libei —
/// and stops there, declaring no barriers.
///
/// The specification is explicit that `ConnectToEIS` "must be invoked before
/// `org.freedesktop.portal.InputCapture.Enable()`", and `set_barriers` is
/// what calls `Enable`. Calling it from here, before `connect_to_eis`, would
/// violate that ordering: the session could be armed and a barrier crossed
/// before any libei device exists to carry the resulting input, so the
/// crossing would switch to the client with nothing able to reach it. The
/// specification also says the EIS connection this function opens is
/// durable — "the same connection can be re-used until the session is
/// closed" across `Disable`/`Enable` — so there is nothing to lose by
/// opening it once, here, and leaving `set_barriers` for `run` (Task 8) to
/// call after the libei handshake has bound the seat's capabilities.
async fn establish() -> Result<(Session, ei::Context)> {
    let portal = Portal::new()
        .await
        .map_err(|e| pe("connecting to the InputCapture portal", e))?;
    let session = portal
        .create_session2(CreateSession2Options::default())
        .await
        .map_err(|e| pe("CreateSession2", e))?;

    let start = portal
        .start(
            &session,
            None,
            StartOptions::default()
                .set_capabilities(Capabilities::Keyboard | Capabilities::Pointer)
                .set_persist_mode(PersistMode::ExplicitlyRevoked),
        )
        .await
        .map_err(|e| pe("Start", e))?
        .response()
        .map_err(|e| pe("Start", e))?;

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
        portal: Rc::new(portal),
        session,
        zones: Vec::new(),
        zone_set: 0,
        sides: Default::default(),
        activation: None,
        armed: false,
    };
    s.refresh_zones().await?;

    let fd = s
        .portal
        .connect_to_eis(&s.session, ConnectToEISOptions::default())
        .await
        .map_err(|e| pe("ConnectToEIS", e))?;
    let stream = UnixStream::from(fd);
    stream
        .set_nonblocking(true)
        .map_err(|e| pe("making the EIS connection non-blocking", e))?;
    let ctx = ei::Context::new(stream).map_err(|e| pe("ei::Context::new", e))?;
    Ok((s, ctx))
}

/// Wraps a D-Bus or libei failure with the name of the call that produced it.
///
/// `ashpd::Error`'s `Display` never names the method in flight, so without
/// `call` a `CreateSession2` failure and a `ConnectToEIS` failure would
/// produce identical text and the log would give no clue which step broke.
fn pe(call: &'static str, e: impl std::fmt::Display) -> Error {
    Error::Backend(format!("portal {call}: {e}"))
}

impl Session {
    async fn refresh_zones(&mut self) -> Result<()> {
        let z = self
            .portal
            .zones(&self.session, GetZonesOptions::default())
            .await
            .map_err(|e| pe("GetZones", e))?
            .response()
            .map_err(|e| pe("GetZones", e))?;
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
    ///
    /// An empty `edges` means "capture nothing": either there is nothing to capture for
    /// yet (this is what `start()` passes, because it runs before any client can
    /// connect) or everything has been withdrawn (the last client went away, or the
    /// input lock came on — spec §7). The first of those has nothing to undo, and
    /// `Disable()` on a session that has never been enabled is a call the specification
    /// neither describes nor promises to accept; failing it would take `start()`, and
    /// therefore `pheme server`, down with it. The second must still go through, or the
    /// barriers stay armed with nothing behind them.
    async fn set_barriers(&mut self, edges: &[CaptureEdge]) -> Result<()> {
        let want = barriers(&self.zones, edges);
        if want.is_empty() && !edges.is_empty() {
            // Not the same thing as "no edges": these edges produced no barrier at all,
            // so nothing will ever activate and the switch will simply never happen.
            // The only way to see it is from here.
            warn!(
                ?edges,
                zones = ?self.zones,
                "no pointer barrier could be placed for these capture edges; \
                 crossing them will do nothing"
            );
        }
        if want.is_empty() && !self.armed {
            self.sides.clear();
            debug!("no capture edges, and nothing armed to withdraw; leaving the session idle");
            return Ok(());
        }
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
            .map_err(|e| pe("SetPointerBarriers", e))?
            .response()
            .map_err(|e| pe("SetPointerBarriers", e))?;

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
            // armed with nothing to trigger it. Reached only when something *was*
            // armed, so the session has been enabled and `Disable` applies.
            self.portal
                .disable(&self.session, DisableOptions::default())
                .await
                .map_err(|e| pe("Disable", e))?;
            self.armed = false;
            return Ok(());
        }
        self.portal
            .enable(&self.session, EnableOptions::default())
            .await
            .map_err(|e| pe("Enable", e))?;
        self.armed = true;
        Ok(())
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

/// What one turn of the dispatch loop picked up.
enum Step {
    /// A command from the backend handle; `None` when every sender is gone.
    Cmd(Option<Cmd>),
    Portal(PortalEvent),
    /// A libei event; `None` when the stream ended.
    Ei(Option<std::result::Result<EiEvent, reis::Error>>),
}

/// One of the four portal signals this session listens to, flattened so the
/// dispatch loop can race them as a single source.
enum PortalEvent {
    Activated {
        id: Option<u32>,
        barrier: Option<ActivatedBarrier>,
        /// Where the compositor says the pointer is. Optional in the protocol.
        position: Option<(f32, f32)>,
    },
    Deactivated {
        id: Option<u32>,
    },
    Disabled,
    ZonesChanged,
    /// A signal stream ended, which means the D-Bus connection or the session
    /// itself is gone. There is nothing left to capture with.
    Closed,
}

/// Races the four portal signal streams.
///
/// Dropping the losing futures is safe: `StreamExt::next` takes an item out of a
/// stream only when it returns `Ready`, so a stream that was polled and answered
/// `Pending` still holds everything it had.
async fn next_portal(
    activated: &mut (impl Stream<Item = Activated> + Unpin),
    deactivated: &mut (impl Stream<Item = Deactivated> + Unpin),
    disabled: &mut (impl Stream<Item = Disabled> + Unpin),
    zones_changed: &mut (impl Stream<Item = ZonesChanged> + Unpin),
) -> PortalEvent {
    use futures_lite::future::or;
    or(
        async {
            match activated.next().await {
                Some(a) => PortalEvent::Activated {
                    id: a.activation_id(),
                    barrier: a.barrier_id(),
                    position: a.cursor_position(),
                },
                None => PortalEvent::Closed,
            }
        },
        or(
            async {
                match deactivated.next().await {
                    Some(d) => PortalEvent::Deactivated {
                        id: d.activation_id(),
                    },
                    None => PortalEvent::Closed,
                }
            },
            or(
                async {
                    match disabled.next().await {
                        Some(_) => PortalEvent::Disabled,
                        None => PortalEvent::Closed,
                    }
                },
                async {
                    match zones_changed.next().await {
                        Some(_) => PortalEvent::ZonesChanged,
                        None => PortalEvent::Closed,
                    }
                },
            ),
        ),
    )
    .await
}

/// Runs one portal session until it is told to stop or the session ends.
///
/// Everything happens on this one thread: `ei::Context` and its event stream are
/// not `Send`, so they are created, used and dropped without ever crossing a
/// thread boundary.
///
/// The order of the first four steps is fixed by the portal specification, which
/// says of `ConnectToEIS` that it "must be invoked before
/// `org.freedesktop.portal.InputCapture.Enable()`": `establish` stops after
/// `ConnectToEIS`, the libei handshake runs here, the signal subscriptions go up
/// before anything can be missed, and only then does `set_barriers` — the one
/// call that ever reaches `Enable` — arm the session.
///
/// `stopped` is signalled once, right after `tx` is dropped at the end of the
/// loop, and before the untimed `shutdown` calls that follow it. `PortalCapture`'s
/// `stop()` waits on it with a bound instead of joining this thread blindly:
/// `shutdown`'s `Release` and `Close` have no timeout of their own, so a slow or
/// hung compositor must not be able to turn `stop()` into an unbounded wait.
pub(crate) async fn run(
    tx: EventSender<CaptureEvent>,
    cmds: async_channel::Receiver<Cmd>,
    screen: Rect,
    mut edges: Vec<CaptureEdge>,
    ready: std::sync::mpsc::Sender<Result<()>>,
    stopped: std::sync::mpsc::Sender<()>,
) {
    // Startup reports its first failure to whoever called `start()`, so a refused
    // permission or a rejected barrier surfaces there instead of leaving a thread
    // that quietly did nothing.
    //
    // This one is for a failure of `establish` itself, which leaves no session
    // behind to close. Every failure *after* it must use `bail_started!`.
    macro_rules! bail {
        ($e:expr) => {{
            let _ = ready.send(Err($e));
            return;
        }};
    }

    let (mut sess, ctx) = match establish().await {
        Ok(v) => v,
        Err(e) => bail!(e),
    };

    // Once `establish` has returned there is a created, started, EIS-connected
    // session, and `ashpd::desktop::Session` has no `Drop` that closes it. Leaving
    // without `shutdown` would hand it to the compositor forever. `set_barriers` is
    // the dangerous one: it can fail *from its own `Enable` call*, on a request the
    // compositor already applied, which arms the barriers and then loses the only
    // thread that reads the libei stream -- the state where the user's keyboard
    // stops coming back.
    //
    // The error goes out before the close, so `start()` is not left waiting on two
    // untimed D-Bus calls.
    macro_rules! bail_started {
        ($sess:expr, $e:expr) => {{
            let _ = ready.send(Err($e));
            shutdown(&mut $sess).await;
            return;
        }};
    }

    let (conn, mut events) = match ctx
        .handshake_async_io("pheme", ei::handshake::ContextType::Receiver)
        .await
    {
        Ok(v) => v,
        Err(e) => bail_started!(
            sess,
            Error::Backend(format!("the libei handshake failed: {e}"))
        ),
    };
    // The handshake hands back a connection handle. The event stream holds a clone
    // of its own, so dropping this one would break nothing; it is kept for the life
    // of the loop because it is the only way to talk back to EIS.
    let _conn = conn;

    // Subscribe before arming anything. An `Activated` that arrives the instant the
    // session is enabled is then queued in its stream rather than missed.
    //
    // Through a handle of their own, so that the streams -- which borrow the proxy
    // for as long as they live -- do not also borrow the `Session` the loop has to
    // mutate.
    let portal = Rc::clone(&sess.portal);
    let mut activated = match portal.receive_activated().await {
        Ok(s) => Box::pin(s),
        Err(e) => bail_started!(sess, pe("subscribing to Activated", e)),
    };
    let mut deactivated = match portal.receive_deactivated().await {
        Ok(s) => Box::pin(s),
        Err(e) => bail_started!(sess, pe("subscribing to Deactivated", e)),
    };
    let mut disabled = match portal.receive_disabled().await {
        Ok(s) => Box::pin(s),
        Err(e) => bail_started!(sess, pe("subscribing to Disabled", e)),
    };
    let mut zones_changed = match portal.receive_zones_changed().await {
        Ok(s) => Box::pin(s),
        Err(e) => bail_started!(sess, pe("subscribing to ZonesChanged", e)),
    };

    if let Err(e) = sess.set_barriers(&edges).await {
        bail_started!(sess, e);
    }
    let _ = ready.send(Ok(()));

    let mut motion = Motion::default();
    let mut held = HeldKeys::default();

    loop {
        // `or` is biased toward its first argument, and that bias is the point:
        // motion events arrived in the thousands within seconds during the design
        // probe, so a `Stop` placed behind them could be starved indefinitely.
        // Commands first, then the portal signals, then libei input.
        //
        // Dropping the two losing futures each turn loses nothing: `next()` removes
        // an item from a stream only when it returns `Ready`, and `recv()` takes a
        // message off the channel only when it returns `Ready` too.
        let step = futures_lite::future::or(
            async { Step::Cmd(cmds.recv().await.ok()) },
            futures_lite::future::or(
                async {
                    Step::Portal(
                        next_portal(
                            &mut activated,
                            &mut deactivated,
                            &mut disabled,
                            &mut zones_changed,
                        )
                        .await,
                    )
                },
                async { Step::Ei(events.next().await) },
            ),
        )
        .await;

        match step {
            // Every sender gone means the backend handle was dropped without a
            // `Stop`; there is nobody left to serve either way.
            Step::Cmd(None) | Step::Cmd(Some(Cmd::Stop)) => break,
            Step::Cmd(Some(Cmd::SetEdges(new, ack))) => {
                edges = new;
                let r = sess.set_barriers(&edges).await;
                if let Err(e) = &r {
                    error!("declaring the new edges failed: {e}");
                }
                // A caller that timed out and dropped its receiver is not an
                // error here; there is nobody left to tell.
                let _ = ack.send(r);
            }
            Step::Cmd(Some(Cmd::Release { x, y, ack })) => {
                let r = release_capture(&mut sess, &mut held, &tx, x, y).await;
                if let Err(e) = &r {
                    error!("releasing the capture failed: {e}");
                }
                let _ = ack.send(r);
            }
            Step::Portal(PortalEvent::Activated {
                id,
                barrier,
                position,
            }) => {
                if id.is_none() {
                    warn!(
                        "the compositor activated capture without an activation id; \
                         Release cannot name it and the capture may not end"
                    );
                }
                sess.activation = id;
                // A new capture starts with no sub-pixel remainder owed to it.
                motion = Motion::default();
                match position {
                    Some((x, y)) => {
                        // The reported position lies outside the zone (measured:
                        // 2577, 2564, 2560 on a 2560-wide screen). Unclamped it
                        // matches no edge and the switch silently never happens.
                        let (cx, cy) = clamp_into(&screen, x, y);
                        debug!(?id, ?barrier, reported = ?(x, y), clamped = ?(cx, cy), "capture activated");
                        // `CaptureActivated`, not `MotionAbs`: the compositor is
                        // already capturing, so the core has to answer — with a switch
                        // or with a release. A `MotionAbs` it decided against would be
                        // answered with nothing, and nothing leaves the user inside a
                        // capture that swallows every key and every motion.
                        if tx
                            .try_send(CaptureEvent::CaptureActivated { x: cx, y: cy })
                            .is_err()
                        {
                            // The core will never answer an event it never received,
                            // so the capture would run on unreleased. Release it here
                            // instead, at the position the core would have chosen.
                            // The centre rather than the reported position: no core
                            // state depends on where this lands, and the centre is
                            // the one point guaranteed not to be on the barrier that
                            // just fired.
                            let (rx, ry) = screen.center();
                            error!("dropped the activation event; releasing the capture");
                            if let Err(e) = release_capture(&mut sess, &mut held, &tx, rx, ry).await
                            {
                                error!("releasing the dropped activation failed: {e}");
                            }
                        }
                    }
                    // Without a position the core cannot tell which edge was
                    // crossed, so there is no switch to start. Say so rather than
                    // letting the pointer stop at the edge for no visible reason.
                    None => warn!(?id, ?barrier, "capture activated with no cursor position"),
                }
            }
            Step::Portal(PortalEvent::Deactivated { id }) => {
                // A `Deactivated` for any other activation is the echo of our own
                // `Release`, which the specification says to expect. One for the
                // *current* activation means the compositor ended the capture
                // itself, which the core cannot learn any other way: it would go on
                // believing it was Remote while the client still held the input.
                if id.is_some() && id == sess.activation {
                    debug!(?id, "the compositor ended the capture");
                    sess.activation = None;
                    // The key-ups for anything still held go to the compositor from
                    // here on, so the core would keep them held forever.
                    for k in held.flush() {
                        if tx
                            .try_send(CaptureEvent::Key {
                                code: k,
                                down: false,
                            })
                            .is_err()
                        {
                            warn!(?k, "dropped a key-up; the core will hold this key");
                        }
                    }
                    // The caller turns this into ServerCore::release_remote(). It is
                    // the only thing that tells the core the compositor ended the
                    // capture, so a drop here is not recoverable the way a dropped
                    // motion event is: the core would stay Remote with the input
                    // going nowhere and nothing saying why.
                    if tx.try_send(CaptureEvent::CaptureEnded).is_err() {
                        error!("dropped CaptureEnded; the core still believes it is capturing");
                    }
                } else {
                    debug!(?id, current = ?sess.activation, "ignoring a Deactivated for another activation");
                }
            }
            Step::Portal(PortalEvent::Disabled) => {
                // No activation to clear and no keys to flush here: the portal
                // specification says of this signal that "if input capturing is
                // currently ongoing, the Deactivated signal is emitted before this
                // signal", and that arm has already done both.
                warn!("the compositor disabled the session; re-enabling");
                if let Err(e) = sess.set_barriers(&edges).await {
                    error!("re-enabling after Disabled failed: {e}");
                }
            }
            Step::Portal(PortalEvent::ZonesChanged) => {
                // The zone set the barriers were declared against is stale now, and
                // `SetPointerBarriers` rejects a stale one.
                if let Err(e) = sess.refresh_zones().await {
                    error!("refreshing zones failed: {e}");
                } else if let Err(e) = sess.set_barriers(&edges).await {
                    error!("re-declaring barriers after a zone change failed: {e}");
                }
            }
            Step::Portal(PortalEvent::Closed) => {
                error!("a portal signal stream ended: the session or the D-Bus connection is gone");
                break;
            }
            Step::Ei(None) => {
                error!("the libei event stream ended: no further input can arrive");
                break;
            }
            Step::Ei(Some(Err(e))) => {
                error!("libei stream error: {e}");
                break;
            }
            Step::Ei(Some(Ok(e))) => match e {
                EiEvent::SeatAdded(s) => bind_seat(&ctx, &s.seat),
                EiEvent::KeyboardModifiers(m) => {
                    // The modifiers held when the capture started, including ones
                    // pressed before it — this is what carries a Shift held across
                    // the edge through to the client.
                    for k in modifier_keys(m.depressed) {
                        held.saw(k, true);
                        let _ = tx.try_send(CaptureEvent::Key {
                            code: k,
                            down: true,
                        });
                    }
                }
                EiEvent::PointerMotion(m) => {
                    let (dx, dy) = motion.push(m.dx, m.dy);
                    if dx != 0 || dy != 0 {
                        let _ = tx.try_send(CaptureEvent::MotionRel { dx, dy });
                    }
                }
                EiEvent::Button(b) => {
                    if let Some(btn) = button_from_evdev(b.button) {
                        let down = matches!(b.state, ei::button::ButtonState::Press);
                        let _ = tx.try_send(CaptureEvent::Button { btn, down });
                    }
                }
                EiEvent::ScrollDiscrete(s) => {
                    let (dx, dy) = wheel_from_discrete(s.discrete_dx, s.discrete_dy);
                    if dx != 0 || dy != 0 {
                        let _ = tx.try_send(CaptureEvent::Wheel { dx, dy });
                    }
                }
                EiEvent::KeyboardKey(k) => {
                    // evdev codes, with no `- 8`: that is the X11 convention.
                    if let Some(code) = key_from_evdev(k.key) {
                        let down = matches!(k.state, ei::keyboard::KeyState::Press);
                        held.saw(code, down);
                        let _ = tx.try_send(CaptureEvent::Key { code, down });
                    }
                }
                EiEvent::Disconnected(d) => {
                    error!(reason = ?d.reason, explanation = ?d.explanation, "the EIS connection was closed");
                    break;
                }
                EiEvent::DeviceAdded(d) => debug!(device = ?d.device, "libei device added"),
                EiEvent::DeviceRemoved(d) => debug!(device = ?d.device, "libei device removed"),
                // Frame is a batch delimiter; ScrollDelta would double-count against
                // ScrollDiscrete; the remaining device lifecycle events need no
                // action in a receiver context.
                _ => {}
            },
        }
    }

    // Dropping `tx` is what lets the receiver observe disconnection, which the
    // trait's `stop()` contract requires -- and it happens *before* `shutdown`,
    // whose two D-Bus calls have no timeout of their own. `stopped` tells
    // `stop()` the moment this has happened, so it can stop waiting on this
    // thread here rather than risk blocking on an unresponsive portal.
    drop(tx);
    let _ = stopped.send(());
    shutdown(&mut sess).await;
}

/// Ends any running capture and closes the portal session.
///
/// Neither happens on its own: `ashpd::desktop::Session` has no `Drop` that calls
/// `Close`, so without this the compositor is left with an enabled session and
/// armed barriers after `stop()` — it would keep capturing at the edge with
/// nothing on this side reading the events.
async fn shutdown(sess: &mut Session) {
    if let Some(id) = sess.activation.take() {
        if let Err(e) = sess
            .portal
            .release(
                &sess.session,
                ReleaseOptions::default().set_activation_id(Some(id)),
            )
            .await
        {
            error!("releasing the capture while shutting down failed: {e}");
        }
    }
    if let Err(e) = sess.session.close().await {
        // Expected when the loop ended because the session was already gone.
        warn!("closing the input capture session failed (it may already have ended): {e}");
    }
}

/// Ends the current capture and puts the pointer back at (x, y).
async fn release_capture(
    sess: &mut Session,
    held: &mut HeldKeys,
    tx: &EventSender<CaptureEvent>,
    x: i32,
    y: i32,
) -> Result<()> {
    // Keys held when the capture ends are never seen being released: the key-up goes
    // to the compositor. Without this the core keeps them held forever.
    for k in held.flush() {
        if tx
            .try_send(CaptureEvent::Key {
                code: k,
                down: false,
            })
            .is_err()
        {
            warn!(?k, "dropped a key-up; the core will hold this key");
        }
    }
    // The id must be the activation being ended: the specification says a compositor
    // ignores a `Release` for an id that is no longer active, so a stale one would
    // leave the capture running.
    let Some(id) = sess.activation.take() else {
        debug!("release with no capture running");
        return Ok(());
    };
    sess.portal
        .release(
            &sess.session,
            ReleaseOptions::default()
                .set_activation_id(Some(id))
                .set_cursor_position(Some((x as f64, y as f64))),
        )
        .await
        .map_err(|e| pe("Release", e))
}
