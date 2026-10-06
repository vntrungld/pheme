//! Sending events through the product's own injectors.
//!
//! The probe emits through `pheme-input`'s uinput and SendInput backends
//! rather than its own, so what pheme's capture sees on the server is what a
//! person's input looks like to it on Linux.
//!
//! Not on Windows, though: pheme's Windows capture deliberately ignores
//! injected input (`LLKHF_INJECTED`), and everything SendInput produces is
//! injected. A Windows *server* never sees this probe's events, so rows with
//! Windows on the server side must emit from outside the guest. The Windows
//! emitter here serves `selfcheck`, whose recorder does see injected input.

use anyhow::{anyhow, Result};
use pheme_input::InputInject;

use crate::gesture::ProbeEvent;

/// One notch, in the 1/120ths `InputInject::wheel` takes.
const NOTCH: i32 = 120;

#[cfg(target_os = "linux")]
fn device_listed(name: &str) -> bool {
    evdev::enumerate().any(|(_, d)| d.name() == Some(name))
}

pub fn send(inj: &mut dyn InputInject, events: &[ProbeEvent]) -> Result<()> {
    for e in events {
        match *e {
            ProbeEvent::MoveRel { dx, dy } => inj.mouse_move_rel(dx, dy),
            ProbeEvent::KeyDown(k) => inj.key(k, true),
            ProbeEvent::KeyUp(k) => inj.key(k, false),
            ProbeEvent::Wheel(n) => inj.wheel(0, n * NOTCH),
        }
        .map_err(|err| anyhow!("injecting {e:?}: {err}"))?;
    }
    Ok(())
}

/// The platform injector, ready to use.
#[cfg(target_os = "linux")]
pub fn open() -> Result<Box<dyn InputInject>> {
    use std::time::{Duration, Instant};

    let inj = pheme_input::linux_uinput::UinputInject::new()
        .map_err(|e| anyhow!("creating the uinput device: {e}"))?;
    // A uinput device exists for its creator before anyone else has opened
    // it, and events sent in that gap go nowhere. Wait until it is listed
    // among the system's input devices -- a condition, against a deadline.
    let deadline = Instant::now() + Duration::from_secs(5);
    while !device_listed(crate::record::PHEME_DEVICE) {
        if Instant::now() >= deadline {
            return Err(anyhow!(
                "the uinput device never appeared among /dev/input devices"
            ));
        }
        std::thread::sleep(Duration::from_millis(50)); // poll interval
    }
    Ok(Box::new(inj))
}

#[cfg(target_os = "windows")]
pub fn open() -> Result<Box<dyn InputInject>> {
    Ok(Box::new(pheme_input::windows::WindowsInject::new()))
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
pub fn open() -> Result<Box<dyn InputInject>> {
    Err(anyhow!("the probe runs on Linux and Windows only"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pheme_input::mock::{InjectCall, MockInject};
    use pheme_proto::KeyCode;

    /// `InputInject::wheel` takes 1/120ths of a notch, and the probe speaks
    /// in notches. The production change that breaks this: pass the notch
    /// count through unconverted. uinput accumulates sub-notch units, so
    /// `wheel -3` would become -3/120 of a notch, nothing would ever scroll,
    /// and row S6 would fail blaming pheme.
    #[test]
    fn a_wheel_notch_is_sent_as_120_units_with_its_sign() {
        let (mut inj, log) = MockInject::new(vec![]);
        send(&mut inj, &[ProbeEvent::Wheel(-3)]).expect("mock never fails");
        assert_eq!(log.calls(), vec![InjectCall::Wheel(0, -360)]);
    }

    #[test]
    fn events_reach_the_injector_in_order() {
        let (mut inj, log) = MockInject::new(vec![]);
        send(
            &mut inj,
            &[
                ProbeEvent::MoveRel { dx: 5, dy: -2 },
                ProbeEvent::KeyDown(KeyCode::LEFT_SHIFT),
                ProbeEvent::KeyUp(KeyCode::LEFT_SHIFT),
            ],
        )
        .expect("mock never fails");
        assert_eq!(
            log.calls(),
            vec![
                InjectCall::MoveRel(5, -2),
                InjectCall::Key(KeyCode::LEFT_SHIFT, true),
                InjectCall::Key(KeyCode::LEFT_SHIFT, false),
            ]
        );
    }
}
