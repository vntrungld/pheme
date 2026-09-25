# Display Input Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A single monitor cabled to both machines follows the pointer: crossing the screen edge switches the monitor's input source to the machine that now has the keyboard and mouse.

**Architecture:** A new crate `pheme-display` wraps `ddc-i2c` (Linux) and `ddc-winapi` (Windows) behind one `Monitor` trait with a mock, and holds a pure `DisplaySwitch` policy that decides whether a crossing becomes a command. In `pheme-app`, a `DisplayService` owns a dedicated `std::thread` carrying the monitor handle, because enumeration costs about a second and an I2C write costs hundreds of milliseconds; the input path only ever does a non-blocking `try_send` to it. The automatic switch hooks into the two sites that already carry the clipboard across the edge, so it needs no new message; the recovery hotkey does need one, because the machine that wants the screen back cannot reach a monitor that is not displaying it.

**Tech Stack:** Rust 2021, `ddc 0.2`, `ddc-i2c 0.2` (Linux), `ddc-winapi 0.2` (Windows), `crossbeam-channel`, `thiserror`, `tracing`, `postcard`, `egui`/`eframe`, `tray-icon`.

**Spec:** `docs/superpowers/specs/2026-09-26-display-switching-design.md`

## Global Constraints

- All documents, code, comments and commit messages in this repository are in **English**.
- Commit format is `{ACTION}: {SHORT_DESCRIPTION}` where ACTION is one of `Update`, `Fix`, `WIP`, `Hotfix`; title under 72 characters, imperative mood; blank line; body wrapped at 72 columns; trailer exactly `Co-Authored-By: Claude <noreply@anthropic.com>` and nothing else.
- `Cargo.lock` is committed with **any** manifest change. `release.yml` builds `--locked`, so an omitted lock file breaks the release job while the CI test job silently regenerates it.
- CI runs `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` on **both** `ubuntu-latest` and `windows-latest`. Code or tests that only compile on one platform break the other leg; guard with `#[cfg(...)]` or `#[cfg_attr(not(target_os = "linux"), allow(dead_code))]`.
- **Nothing may block the input path.** Every call the router thread makes into this sub-project's code is a non-blocking `try_send` or a field read.
- `ddc-hi` is not a dependency. Only `ddc`, `ddc-i2c` and `ddc-winapi`.
- `ddc-i2c` is declared `default-features = false, features = ["with-linux"]`; its `with-linux-enumerate` feature pulls `udev`/`libudev-sys` and must stay off.
- VCP feature code for Input Select is `0x60` (`pheme_display::INPUT_SELECT`).
- `PROTOCOL_VERSION` becomes `3`.
- Default `cooldown_ms` is `1000`.
- The feature is **off** unless `[display] input` is set in the config.
- `enumerate()` costs about 1.09 s; it runs only on `DisplayService`'s own thread.
- Windows arms of `pheme-app`, `pheme-net` and `pheme-display` cannot be cross-checked locally (they reach `ring` via `quinn`, or need `x86_64-w64-mingw32-gcc`). `pheme-display` alone *can* be checked with:
  `RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone PATH=$HOME/.cargo-standalone/bin:$PATH cargo check -p pheme-display --target x86_64-pc-windows-gnu --all-targets`

## Review Focus

These are the failure modes the spec implies that no task's own deliverable would naturally exercise. Each has a test pinned to the task that owns the code.

1. **Both machines configured with the same `display.input`.** The most likely misconfiguration: crossing the edge commands the monitor to the input it is already on, so nothing ever visibly happens and nothing says why. Expected: one warning at handshake naming both values. — Task 8.
2. **`cooldown_ms = 0`.** Rule 2 holds nothing, so `deadline()` must never hand the service thread a wakeup time that is already past in a loop that immediately re-arms it. Expected: no busy spin, requests issue immediately. — Task 1 (policy) and Task 7 (service).
3. **An enormous `cooldown_ms`.** `Instant + Duration` panics on overflow and `cooldown_ms` is a `u64` a person can set to anything. Expected: no panic; the deadline simply never arrives. — Task 1.
4. **`Msg::SwitchDisplay` arriving while the local display feature is off.** A peer with a monitor sends it to a peer without one. Expected: a no-op, not an unwrap on a `None` service. — Task 8 and Task 9.
5. **An EDID descriptor whose text is unterminated, padded to the full 13 bytes, or non-ASCII.** Expected: no panic and no control characters in the identity, because that identity is printed and substring-matched. — Task 2.

---

## File Structure

**Created:**

| File | Responsibility |
|---|---|
| `crates/pheme-display/Cargo.toml` | the new crate's manifest |
| `crates/pheme-display/src/lib.rs` | `DisplayError`, `Monitor` trait, `INPUT_SELECT`, `enumerate`, `open` |
| `crates/pheme-display/src/switch.rs` | `DisplaySwitch`, the pure policy |
| `crates/pheme-display/src/edid.rs` | `identity_from_edid` |
| `crates/pheme-display/src/caps.rs` | `input_values_from_caps` |
| `crates/pheme-display/src/mock.rs` | `MockMonitor`, `MockMonitorHandle` |
| `crates/pheme-display/src/backend_i2c.rs` | Linux `/dev/i2c-*` backend |
| `crates/pheme-display/src/backend_winapi.rs` | Windows Dxva2 backend |
| `crates/pheme-app/src/display.rs` | `DisplayService`, `OpenFn`, the service thread |
| `crates/pheme-app/tests/display.rs` | service-level tests over `MockMonitor` |

**Modified:**

| File | Change |
|---|---|
| `Cargo.toml` | workspace member, `pheme-display` and the three `ddc*` dependencies |
| `crates/pheme-proto/src/lib.rs` | `display_input` on `Hello`/`HelloAck`, `Msg::SwitchDisplay`, version 3 |
| `crates/pheme-core/src/server.rs` | `Hotkeys.switch_display`, `Action::SwitchDisplay { local }` |
| `crates/pheme-app/src/config.rs` | `DisplayCfg`, `HotkeysCfg.switch_display` |
| `crates/pheme-app/src/server.rs` | `Link.display_input`, the `Enter` hook, both `SwitchDisplay` paths |
| `crates/pheme-app/src/client.rs` | display service, the `Leave` hook, `HelloAck`, `SwitchDisplay` |
| `crates/pheme-app/src/main.rs` | `pheme displays` |
| `crates/pheme-app/src/setup.rs` | i2c udev rule and module |
| `crates/pheme-app/src/lib.rs` | `pub mod display;` |
| `crates/pheme-app/src/ipc/proto.rs` | `Command::SwitchDisplay` |
| `crates/pheme-app/src/frontend/{window,tray}.rs` | button, tray item, status line |
| `README.md`, `docs/testing.md` | `[display]` documentation, rows E1–E9 |
| `docs/superpowers/specs/2026-09-21-pheme-architecture-design.md` | already updated in the spec commit |

---

### Task 1: The `pheme-display` crate and the `DisplaySwitch` policy

**Files:**
- Create: `crates/pheme-display/Cargo.toml`
- Create: `crates/pheme-display/src/lib.rs`
- Create: `crates/pheme-display/src/switch.rs`
- Modify: `Cargo.toml` (workspace members and dependencies)

**Interfaces:**
- Consumes: nothing.
- Produces: `pheme_display::{DisplayError, Monitor, INPUT_SELECT}` and `pheme_display::switch::DisplaySwitch` with `new(Duration)`, `observe(u16)`, `request(u16, Instant) -> Option<u16>`, `force(u16, Instant) -> u16`, `poll(Instant) -> Option<u16>`, `deadline() -> Option<Instant>`, `confirm(u16)`, `forget()`.

The backends are **not** in this task. `enumerate` and `open` arrive in Task 3; this task defines the trait they implement and the policy they are driven by.

- [ ] **Step 1: Add the crate to the workspace**

In the root `Cargo.toml`, add `"crates/pheme-display",` to `members` after `"crates/pheme-clip",`, and these lines to `[workspace.dependencies]` — `pheme-display` beside the other path dependencies, the `ddc*` three beside `arboard`:

```toml
pheme-display = { path = "crates/pheme-display" }

ddc = "0.2"
ddc-i2c = { version = "0.2", default-features = false, features = ["with-linux"] }
ddc-winapi = "0.2"
```

`default-features = false` on `ddc-i2c` is load-bearing: its `with-linux-enumerate` feature pulls `udev` and `libudev-sys`, a native C dependency this project does not want.

- [ ] **Step 2: Write the crate manifest**

`crates/pheme-display/Cargo.toml`:

```toml
[package]
name = "pheme-display"
version.workspace = true
edition.workspace = true
license.workspace = true
rust-version.workspace = true

[dependencies]
thiserror = { workspace = true }
tracing = { workspace = true }
ddc = { workspace = true }

[target.'cfg(target_os = "linux")'.dependencies]
ddc-i2c = { workspace = true }

[target.'cfg(windows)'.dependencies]
ddc-winapi = { workspace = true }
```

- [ ] **Step 3: Write the failing test**

Create `crates/pheme-display/src/switch.rs` containing only this test module, so the file compiles to a failing test rather than to nothing:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    const COOLDOWN: Duration = Duration::from_millis(1000);

    fn at(base: Instant, ms: u64) -> Instant {
        base + Duration::from_millis(ms)
    }

    /// Rule 1. Break it by deleting the `self.selected == Some(value)` arm
    /// of `request`: the second call then returns `Some(0x11)`.
    #[test]
    fn a_repeat_of_the_selected_input_is_refused() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x11, at(t0, 5_000)), None);
    }

    /// Rule 2. Break it by making `in_cooldown` return `false` always: the
    /// second request then issues immediately instead of being held.
    #[test]
    fn a_second_request_inside_the_cooldown_is_held_not_issued() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, at(t0, 100)), None);
        assert_eq!(s.poll(at(t0, 100)), None);
        assert_eq!(s.poll(at(t0, 1_001)), Some(0x0f));
    }

    /// Rule 3, the part a leading-edge throttle gets wrong. Break it by
    /// making `request` drop the value instead of assigning `self.pending`:
    /// the final `poll` then returns `None` and the monitor stays on the
    /// machine the pointer left.
    #[test]
    fn the_held_value_is_the_most_recent_one() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, at(t0, 100)), None);
        assert_eq!(s.request(0x12, at(t0, 200)), None);
        assert_eq!(s.poll(at(t0, 1_001)), Some(0x12));
    }

    /// Rule 3's tail: brushing the edge and ending where you started costs
    /// no command. Break it by deleting the `self.selected == Some(v)`
    /// check in `poll`: it then returns `Some(0x0f)` and re-commands the
    /// input the monitor is already showing, mid-switch.
    ///
    /// Note the two held requests. A version of this test that lets rule 1
    /// answer the second call never sets `pending` at all, so `poll` returns
    /// `None` because there is nothing held -- and the assertion passes with
    /// the discard clause deleted.
    #[test]
    fn a_held_value_that_matches_the_selection_is_discarded() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x0f, t0), Some(0x0f));
        s.confirm(0x0f);
        // Brushed back to the server, then straight out to the client
        // again, both inside the cooldown.
        assert_eq!(s.request(0x11, at(t0, 100)), None);
        assert_eq!(s.request(0x0f, at(t0, 200)), None);
        assert_eq!(s.poll(at(t0, 1_001)), None);
    }

    /// The `ClipSync` defect, in its display form. Break it by making
    /// `forget` a no-op: the retry is then refused forever and the monitor
    /// can never be corrected.
    #[test]
    fn forget_lets_an_identical_retry_through() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x11, at(t0, 2_000)), None);
        s.forget();
        assert_eq!(s.request(0x11, at(t0, 4_000)), Some(0x11));
    }

    /// Break it by routing `force` through `request`: it then returns
    /// `None` on the deduplicated value and the recovery path dies.
    #[test]
    fn force_ignores_both_the_dedupe_and_the_cooldown() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(COOLDOWN);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.force(0x11, at(t0, 10)), 0x11);
    }

    /// Review Focus 2. Break it by making `in_cooldown` return `true` when
    /// `cooldown` is zero: the second request is then held with a deadline
    /// already in the past, which the service thread re-arms forever.
    #[test]
    fn a_zero_cooldown_holds_nothing() {
        let t0 = Instant::now();
        let mut s = DisplaySwitch::new(Duration::ZERO);
        assert_eq!(s.request(0x11, t0), Some(0x11));
        s.confirm(0x11);
        assert_eq!(s.request(0x0f, t0), Some(0x0f));
        assert_eq!(s.deadline(), None);
    }

    /// Review Focus 3. `cooldown_ms` is a `u64` a person can set to
    /// anything, and `Instant + Duration` panics on overflow. Break it by
    /// writing `t + self.cooldown` in `deadline`.
    ///
    /// Two magnitudes, because they take different branches. The largest
    /// value the config can actually produce -- `from_millis(u64::MAX)`,
    /// about 584 million years -- is still well inside `Instant`'s range on
    /// both Linux and Windows, so the deadline is real and simply never
    /// arrives. `Duration::MAX` is outside it, and that is the branch
    /// `checked_add` exists for.
    #[test]
    fn an_enormous_cooldown_does_not_panic() {
        for (cooldown, deadline_exists) in [
            (Duration::from_millis(u64::MAX), true),
            (Duration::MAX, false),
        ] {
            let t0 = Instant::now();
            let mut s = DisplaySwitch::new(cooldown);
            assert_eq!(s.request(0x11, t0), Some(0x11));
            s.confirm(0x11);
            assert_eq!(s.request(0x0f, at(t0, 1)), None);
            assert_eq!(s.deadline().is_some(), deadline_exists, "{cooldown:?}");
        }
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p pheme-display`
Expected: FAIL — `cannot find type DisplaySwitch in this scope`.

- [ ] **Step 5: Write the policy**

Prepend to `crates/pheme-display/src/switch.rs`, above the test module:

```rust
//! The policy that decides whether a pointer crossing becomes a DDC/CI
//! command.

use std::time::{Duration, Instant};

/// Decides whether a request to select a monitor input becomes a command.
///
/// Pure: no I/O, and no clock of its own -- `Instant` is passed in -- so
/// every rule is a unit test that needs no monitor. The same shape as
/// `pheme_clip::ClipSync`, for the same reason.
///
/// Three rules:
///
/// 1. Never command the input already selected.
/// 2. Never command twice inside the cooldown. Monitors take one to three
///    seconds to switch and re-sync, and a command arriving mid-switch is at
///    best ignored.
/// 3. A request made during the cooldown is *held*, not dropped. The most
///    recent one wins, and it is discarded if by the time it comes due it
///    equals what is selected.
///
/// Rule 3 has to be built this way. A leading-edge throttle -- the obvious
/// implementation -- drops the second request, so brushing the edge and
/// coming straight back would leave the monitor showing the machine the
/// pointer is no longer on, permanently.
#[derive(Debug)]
pub struct DisplaySwitch {
    cooldown: Duration,
    /// The input the monitor is believed to be showing. `None` means
    /// unknown, which is also what a failed command leaves behind.
    selected: Option<u16>,
    /// When the last command was handed out.
    last_at: Option<Instant>,
    /// A value asked for during the cooldown and not yet handed out.
    pending: Option<u16>,
}

impl DisplaySwitch {
    pub fn new(cooldown: Duration) -> Self {
        DisplaySwitch {
            cooldown,
            selected: None,
            last_at: None,
            pending: None,
        }
    }

    /// Seed `selected` from a successful read, so the first crossing to the
    /// input already showing costs no command.
    pub fn observe(&mut self, value: u16) {
        self.selected = Some(value);
    }

    /// A crossing asks for `value`. `Some(v)` means issue it now.
    pub fn request(&mut self, value: u16, now: Instant) -> Option<u16> {
        // Rule 1. Skipped while something is pending, so that asking for the
        // selected input can *cancel* a held request rather than being
        // ignored beside it: `poll` discards it on the same comparison.
        if self.pending.is_none() && self.selected == Some(value) {
            return None;
        }
        // Rules 2 and 3.
        if self.in_cooldown(now) {
            self.pending = Some(value);
            return None;
        }
        Some(self.issue(value, now))
    }

    /// The recovery hotkey (design §8): ignores rules 1 and 2, because its
    /// whole purpose is to correct a monitor whose state the policy has
    /// wrong.
    pub fn force(&mut self, value: u16, now: Instant) -> u16 {
        self.issue(value, now)
    }

    /// Hand out a held request that has come due.
    pub fn poll(&mut self, now: Instant) -> Option<u16> {
        let v = self.pending?;
        if self.in_cooldown(now) {
            return None;
        }
        self.pending = None;
        if self.selected == Some(v) {
            // The state converged while the request was held -- the pointer
            // left and came back -- so there is nothing to command.
            return None;
        }
        Some(self.issue(v, now))
    }

    /// When `poll` could next return something, or `None` when nothing is
    /// held.
    ///
    /// `checked_add` rather than `+`: `Instant + Duration` panics on
    /// overflow, and `cooldown` comes from a config file where `cooldown_ms`
    /// is a `u64` a person can set to anything. An overflowing deadline
    /// reads as "never", which is what that configuration asked for.
    pub fn deadline(&self) -> Option<Instant> {
        self.pending?;
        self.last_at?.checked_add(self.cooldown)
    }

    /// The command the policy handed out reached the monitor.
    pub fn confirm(&mut self, value: u16) {
        self.selected = Some(value);
    }

    /// It did not. `selected` becomes unknown so rule 1 cannot refuse the
    /// retry.
    ///
    /// Sub-project 5 shipped this defect in `ClipSync`, which recorded text
    /// before the write carrying it succeeded and so refused every retry of
    /// the same text forever. Here it would be worse: `selected` would name
    /// an input the monitor is not showing, and rule 1 would refuse to
    /// correct it.
    pub fn forget(&mut self) {
        self.selected = None;
    }

    fn issue(&mut self, value: u16, now: Instant) -> u16 {
        self.last_at = Some(now);
        self.pending = None;
        value
    }

    fn in_cooldown(&self, now: Instant) -> bool {
        match self.last_at {
            // `saturating_duration_since` rather than subtraction: a caller
            // passing a stale `Instant` should get "not in cooldown", never a
            // panic. A zero cooldown is never "in" it, which is what makes
            // `cooldown_ms = 0` mean "switch every time".
            Some(t) => now.saturating_duration_since(t) < self.cooldown,
            None => false,
        }
    }
}
```

- [ ] **Step 6: Write the crate root**

`crates/pheme-display/src/lib.rs`:

```rust
//! Monitor input switching over DDC/CI.
//!
//! One trait, two backends and a policy. The crate knows nothing about
//! pheme's protocol or its configuration: it is handed a number and told to
//! put it in the monitor's Input Select register.

pub mod switch;

pub use switch::DisplaySwitch;

/// VCP feature code for Input Select (MCCS 2.2 section 8.4).
pub const INPUT_SELECT: u8 = 0x60;

#[derive(Debug, thiserror::Error)]
pub enum DisplayError {
    /// Nothing on this machine answered a read of VCP 0x60.
    #[error("no monitor answered DDC/CI")]
    NoMonitor,
    /// Monitors were found but none matched `display.monitor`.
    #[error("no monitor matches {0:?}")]
    NoMatch(String),
    #[error("DDC/CI: {0}")]
    Backend(String),
}

/// One monitor that answers DDC/CI.
///
/// Deliberately **not** `Send`. `ddc_winapi::Monitor` wraps a
/// `PHYSICAL_MONITOR`, which holds a raw `HANDLE` and carries no
/// `unsafe impl Send`, so a `Send` bound here would fail to compile on
/// Windows and nowhere else -- the CI leg this repository cannot run
/// locally. It does not need one: the handle is created by the `OpenFn`
/// *on* `DisplayService`'s own thread and never leaves it. A closure's
/// `Send` depends on what it captures, not on what it returns, so the
/// boxed `OpenFn` stays `Send` regardless.
pub trait Monitor {
    /// A stable, human-readable name. Matched case-insensitively against
    /// `display.monitor`, and printed by `pheme displays`.
    fn identity(&self) -> &str;
    /// Where the backend found it: an i2c device path on Linux, the physical
    /// monitor description on Windows. Printed, never matched.
    fn location(&self) -> &str;
    fn get_input(&mut self) -> Result<u16, DisplayError>;
    fn set_input(&mut self, value: u16) -> Result<(), DisplayError>;
    /// The raw capability string, when the monitor returns one.
    fn capabilities(&mut self) -> Result<String, DisplayError>;
}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pheme-display`
Expected: PASS, 8 tests.

- [ ] **Step 8: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```
Expected: all clean. If clippy objects that `Monitor` has no implementors yet, it is `dead_code` on the trait — do **not** silence it; Task 3 adds the implementors, and the trait is `pub` in a library crate so it is reachable and clippy will not fire.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock crates/pheme-display
git commit -F - <<'EOF'
Update: add pheme-display with the input switching policy

The crate that will carry DDC/CI monitor control, and the pure policy
that decides whether a pointer crossing becomes a command. Backends
arrive in a later commit; this defines the trait they implement.

DisplaySwitch holds three rules. It never commands the input already
selected, never commands twice inside the cooldown, and -- the one that
has to be built deliberately -- holds a request made during the cooldown
rather than dropping it. A leading-edge throttle, the obvious
implementation, would leave the monitor stuck showing the machine the
pointer just left whenever someone brushes the edge and comes back.

confirm/forget exist because sub-project 5 shipped the same defect in
ClipSync: it recorded text before the write carrying it succeeded, so one
failure refused every later retry. Here the policy records a selected
input only after the command succeeds.

ddc-i2c is declared with default-features = false so its
with-linux-enumerate feature, and the libudev-sys dependency behind it,
stay out of the build.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 2: EDID identity and capability-string parsing

**Files:**
- Create: `crates/pheme-display/src/edid.rs`
- Create: `crates/pheme-display/src/caps.rs`
- Modify: `crates/pheme-display/src/lib.rs` (declare both modules)

**Interfaces:**
- Consumes: nothing from Task 1 but the crate itself.
- Produces: `pheme_display::edid::identity_from_edid(&[u8]) -> Option<String>` and `pheme_display::caps::input_values_from_caps(&str) -> Vec<u16>`.

Both exist so the crate does not depend on the `edid` crate, which pulls `nom 3.2.1` — code cargo already reports "will be rejected by a future version of Rust".

- [ ] **Step 1: Write the failing tests**

Create `crates/pheme-display/src/edid.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a valid 128-byte EDID with the given display descriptors.
    /// `descriptors` is a list of `(tag, text)` placed at offsets 54, 72, 90
    /// and 108 in order; unused slots become dummy descriptors.
    fn edid_with(manufacturer: [u8; 2], descriptors: &[(u8, &[u8])]) -> Vec<u8> {
        let mut e = vec![0u8; 128];
        e[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        e[8] = manufacturer[0];
        e[9] = manufacturer[1];
        for (i, off) in [54usize, 72, 90, 108].iter().enumerate() {
            let (tag, text) = descriptors.get(i).copied().unwrap_or((0x10, &[][..]));
            e[*off] = 0;
            e[off + 1] = 0;
            e[off + 2] = 0;
            e[off + 3] = tag;
            e[off + 4] = 0;
            for j in 0..13 {
                e[off + 5 + j] = *text.get(j).unwrap_or(&0x20);
            }
        }
        // EDID checksum: the 128 bytes must sum to 0 mod 256.
        let sum: u8 = e[..127].iter().fold(0u8, |a, b| a.wrapping_add(*b));
        e[127] = 0u8.wrapping_sub(sum);
        e
    }

    /// Break it by dropping the serial branch: the identity loses
    /// " (106NTMXE1579)".
    #[test]
    fn a_name_and_a_serial_become_one_identity() {
        let e = edid_with(
            // "GSM" -> 0b00111_10011_01101 -> 0x1E6D
            [0x1E, 0x6D],
            &[(0xFC, b"LG ULTRAGEAR\n"), (0xFF, b"106NTMXE1579\n")],
        );
        assert_eq!(
            identity_from_edid(&e).as_deref(),
            Some("GSM LG ULTRAGEAR (106NTMXE1579)")
        );
    }

    /// Break it by returning the manufacturer unconditionally instead of
    /// appending only the descriptors that exist: a trailing " ()" appears.
    #[test]
    fn a_manufacturer_alone_is_still_an_identity() {
        let e = edid_with([0x1E, 0x6D], &[]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM"));
    }

    /// Break it by deleting the header check: a block of zeroes then parses
    /// as a display named "@@@".
    #[test]
    fn a_bad_header_is_refused() {
        let mut e = edid_with([0x1E, 0x6D], &[(0xFC, b"X\n")]);
        e[1] = 0x00;
        assert_eq!(identity_from_edid(&e), None);
    }

    /// Break it by deleting the checksum check: the corrupted block parses.
    #[test]
    fn a_bad_checksum_is_refused() {
        let mut e = edid_with([0x1E, 0x6D], &[(0xFC, b"X\n")]);
        e[127] = e[127].wrapping_add(1);
        assert_eq!(identity_from_edid(&e), None);
    }

    /// Break it by indexing `edid[54..]` without a length check: this
    /// panics instead of returning None.
    #[test]
    fn a_short_block_is_refused() {
        assert_eq!(identity_from_edid(&[0x00, 0xFF]), None);
    }

    /// Review Focus 5: text that fills all 13 bytes with no 0x0A, and text
    /// carrying a byte outside printable ASCII. Break it by trimming on
    /// 0x0A alone, or by using `String::from_utf8_lossy` with no filter:
    /// the identity then carries a replacement character or a control byte
    /// into a string that gets printed and substring-matched.
    #[test]
    fn descriptor_text_is_trimmed_and_kept_printable() {
        let e = edid_with([0x1E, 0x6D], &[(0xFC, b"ABCDEFGHIJKLM")]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM ABCDEFGHIJKLM"));

        let e = edid_with([0x1E, 0x6D], &[(0xFC, b"AB\x01\x7fCD\n")]);
        assert_eq!(identity_from_edid(&e).as_deref(), Some("GSM ABCD"));
    }
}
```

Create `crates/pheme-display/src/caps.rs` with only this test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// Break it by scanning for "60(" anywhere rather than inside `vcp(`:
    /// the "60" inside "prot(monitor)" style text, or a model number, would
    /// be read as an input list.
    #[test]
    fn the_input_list_comes_from_the_vcp_section() {
        let caps = "(prot(monitor)type(LCD)model(X60)cmds(01 02 03)\
                    vcp(02 10 12 14(05 08) 60(0F 11 12) AC)mccs_ver(2.1))";
        assert_eq!(input_values_from_caps(caps), vec![0x0F, 0x11, 0x12]);
    }

    /// Break it by unwrapping the result of the `60(` search: a monitor
    /// whose capability string lists no input feature then panics
    /// `pheme displays`.
    #[test]
    fn a_string_without_the_input_feature_yields_nothing() {
        let caps = "(prot(monitor)type(LCD)vcp(02 10 12)mccs_ver(2.1))";
        assert!(input_values_from_caps(caps).is_empty());
    }

    /// Break it by reading to the end of the string when no ')' follows:
    /// the truncated list then yields values from whatever text came after.
    #[test]
    fn a_truncated_list_yields_nothing() {
        let caps = "(prot(monitor)vcp(02 60(0F 11";
        assert!(input_values_from_caps(caps).is_empty());
    }

    /// Break it by parsing decimal: 0x11 would come back as 11.
    #[test]
    fn values_are_hexadecimal() {
        assert_eq!(input_values_from_caps("vcp(60(10 11))"), vec![0x10, 0x11]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-display`
Expected: FAIL — `cannot find function identity_from_edid in this scope` and the same for `input_values_from_caps`. (Add `pub mod edid;` and `pub mod caps;` to `lib.rs` first if the files are not yet reached.)

- [ ] **Step 3: Implement the EDID reader**

Prepend to `crates/pheme-display/src/edid.rs`:

```rust
//! Just enough EDID to name a monitor.
//!
//! The `edid` crate would do this, and would pull `nom 3.2.1` into the
//! build -- code cargo already reports as "will be rejected by a future
//! version of Rust". The two fields needed sit at fixed offsets, so this
//! reads them directly. Layout is EDID 1.3/1.4, which has not moved since
//! 2006.

/// Offsets of the four 18-byte descriptor blocks.
const DESCRIPTORS: [usize; 4] = [54, 72, 90, 108];
/// Descriptor tag for the monitor name.
const TAG_NAME: u8 = 0xFC;
/// Descriptor tag for the serial number.
const TAG_SERIAL: u8 = 0xFF;

/// A display name built from a raw EDID base block: the manufacturer id
/// (bytes 8-9), the descriptor tagged 0xFC (monitor name) and the one
/// tagged 0xFF (serial number).
///
/// `None` when the block is shorter than 128 bytes, the 8-byte header is
/// not `00 FF FF FF FF FF FF 00`, or the bytes do not sum to 0 mod 256.
pub fn identity_from_edid(edid: &[u8]) -> Option<String> {
    let block = edid.get(..128)?;
    if block[..8] != [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00] {
        return None;
    }
    if block.iter().fold(0u8, |a, b| a.wrapping_add(*b)) != 0 {
        return None;
    }
    let mut out = manufacturer(block[8], block[9]);
    if let Some(name) = descriptor_text(block, TAG_NAME) {
        out.push(' ');
        out.push_str(&name);
    }
    if let Some(serial) = descriptor_text(block, TAG_SERIAL) {
        out.push_str(" (");
        out.push_str(&serial);
        out.push(')');
    }
    Some(out)
}

/// Bytes 8-9 are three 5-bit letters, big-endian, with `A` = 1.
fn manufacturer(hi: u8, lo: u8) -> String {
    let v = u16::from_be_bytes([hi, lo]);
    (0..3)
        .map(|i| {
            let five = ((v >> (10 - 5 * i)) & 0x1F) as u8;
            // 0 is not a letter; anything out of range becomes '?' rather
            // than a control character, because this string is printed.
            if (1..=26).contains(&five) {
                (b'A' + five - 1) as char
            } else {
                '?'
            }
        })
        .collect()
}

/// The text of the display descriptor carrying `tag`, or `None`.
///
/// A display descriptor has three leading zero bytes; byte 3 is the tag and
/// bytes 5..18 the text, ended by 0x0A and padded with 0x20. Bytes outside
/// printable ASCII are dropped rather than replaced: the result is printed
/// by `pheme displays` and substring-matched against `display.monitor`, and
/// neither wants a control character in it.
fn descriptor_text(block: &[u8], tag: u8) -> Option<String> {
    for off in DESCRIPTORS {
        let d = &block[off..off + 18];
        if d[0] != 0 || d[1] != 0 || d[2] != 0 || d[3] != tag {
            continue;
        }
        let text: String = d[5..18]
            .iter()
            .take_while(|b| **b != 0x0A)
            .filter(|b| (0x20..0x7F).contains(*b))
            .map(|b| *b as char)
            .collect();
        let text = text.trim().to_string();
        if !text.is_empty() {
            return Some(text);
        }
    }
    None
}
```

- [ ] **Step 4: Implement the capability reader**

Prepend to `crates/pheme-display/src/caps.rs`:

```rust
//! The input values a monitor claims to accept.
//!
//! A DDC/CI capability string looks like
//! `(prot(monitor)type(LCD)vcp(02 10 60(0F 11 12) AC)mccs_ver(2.1))`. The
//! `vcp(...)` section lists feature codes, and a code followed by
//! parentheses lists the values it accepts. Feature 60 is Input Select.
//!
//! Advisory only: plenty of monitors return nothing, or a list that omits
//! inputs they do accept. `pheme displays` prints it as a hint beside the
//! input actually selected, never as the truth.

use crate::INPUT_SELECT;

/// The values listed for VCP feature 0x60 inside the `vcp(...)` section, or
/// an empty vector when the string has no such list.
pub fn input_values_from_caps(caps: &str) -> Vec<u16> {
    let Some(vcp) = section(caps, "vcp(") else {
        return Vec::new();
    };
    let needle = format!("{INPUT_SELECT:02X}(");
    let start = match find_ignore_case(&vcp, &needle) {
        Some(i) => i + needle.len(),
        None => return Vec::new(),
    };
    // A list with no closing parenthesis is a truncated string, not a list
    // that runs to the end: reading on would take values out of whatever
    // text follows.
    let Some(end) = vcp[start..].find(')') else {
        return Vec::new();
    };
    vcp[start..start + end]
        .split_whitespace()
        .filter_map(|t| u16::from_str_radix(t, 16).ok())
        .collect()
}

/// The text between `open` and its matching close parenthesis.
fn section<'a>(caps: &'a str, open: &str) -> Option<&'a str> {
    let start = find_ignore_case(caps, open)? + open.len();
    let mut depth = 1usize;
    for (i, c) in caps[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(&caps[start..start + i]);
                }
            }
            _ => {}
        }
    }
    None
}

fn find_ignore_case(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_uppercase().find(&needle.to_ascii_uppercase())
}
```

- [ ] **Step 5: Declare both modules**

In `crates/pheme-display/src/lib.rs`, below `pub mod switch;`:

```rust
pub mod caps;
pub mod edid;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p pheme-display`
Expected: PASS, 18 tests.

- [ ] **Step 7: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 8: Commit**

```bash
git add crates/pheme-display
git commit -F - <<'EOF'
Update: read a monitor's name and input list without the edid crate

identity_from_edid takes the manufacturer id and the 0xFC and 0xFF
descriptors out of a raw EDID base block; input_values_from_caps takes
the values listed for VCP 0x60 out of a DDC/CI capability string.

Both exist to keep the edid crate out of the build, because it depends
on nom 3.2.1, which cargo reports as code that will be rejected by a
future version of Rust. The EDID layout they read has not changed since
2006 and fits in sixty lines.

Descriptor text is filtered to printable ASCII rather than merely
trimmed. That string is printed by `pheme displays` and substring
matched against display.monitor, so a control byte in it would be a
problem in two places at once.

A capability list with no closing parenthesis yields nothing rather than
reading to the end of the string, which would take values out of
whatever text followed the truncation.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 3: The mock, the two backends, and `enumerate`/`open`

**Files:**
- Create: `crates/pheme-display/src/mock.rs`
- Create: `crates/pheme-display/src/backend_i2c.rs`
- Create: `crates/pheme-display/src/backend_winapi.rs`
- Modify: `crates/pheme-display/src/lib.rs`

**Interfaces:**
- Consumes: `Monitor`, `DisplayError`, `INPUT_SELECT` (Task 1); `identity_from_edid` (Task 2).
- Produces: `pheme_display::enumerate() -> Vec<Box<dyn Monitor>>`, `pheme_display::open(Option<&str>) -> Result<Box<dyn Monitor>, DisplayError>`, and `pheme_display::mock::{MockMonitor, MockMonitorHandle}` with `MockMonitor::new(identity, location, input) -> (MockMonitor, MockMonitorHandle)`, handle methods `input() -> u16`, `sets() -> usize`, `fail_with(&str)`, `stop_failing()`, `set_caps(&str)`.

The Windows backend cannot be run here; it can be compiled, and the last step does so. Do not skip that step — two Windows-only CI breaks reached `master` during sub-project 6.

- [ ] **Step 1: Write the mock**

`crates/pheme-display/src/mock.rs`:

```rust
//! An in-memory monitor for tests.
//!
//! The handle is cloneable and readable from the test thread while the
//! `MockMonitor` itself is owned by the service thread, the same split
//! `pheme_clip::mock` uses.

use std::sync::{Arc, Mutex};

use crate::{DisplayError, Monitor};

#[derive(Debug)]
struct Inner {
    input: u16,
    /// Every call to `set_input`, successful or not.
    sets: usize,
    fail: Option<String>,
    caps: String,
}

#[derive(Clone, Debug)]
pub struct MockMonitorHandle(Arc<Mutex<Inner>>);

impl MockMonitorHandle {
    /// The input last set *successfully*. Distinct from `sets`, which counts
    /// attempts: a test that asserts only on one of them cannot tell a
    /// refused command from a failed one.
    pub fn input(&self) -> u16 {
        self.0.lock().unwrap().input
    }

    /// How many times `set_input` was called, including calls that failed.
    pub fn sets(&self) -> usize {
        self.0.lock().unwrap().sets
    }

    pub fn fail_with(&self, message: &str) {
        self.0.lock().unwrap().fail = Some(message.to_string());
    }

    pub fn stop_failing(&self) {
        self.0.lock().unwrap().fail = None;
    }

    pub fn set_caps(&self, caps: &str) {
        self.0.lock().unwrap().caps = caps.to_string();
    }
}

pub struct MockMonitor {
    identity: String,
    location: String,
    inner: Arc<Mutex<Inner>>,
}

impl MockMonitor {
    pub fn new(identity: &str, location: &str, input: u16) -> (Self, MockMonitorHandle) {
        let inner = Arc::new(Mutex::new(Inner {
            input,
            sets: 0,
            fail: None,
            caps: String::new(),
        }));
        let mon = MockMonitor {
            identity: identity.to_string(),
            location: location.to_string(),
            inner: Arc::clone(&inner),
        };
        (mon, MockMonitorHandle(inner))
    }
}

impl Monitor for MockMonitor {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn location(&self) -> &str {
        &self.location
    }

    fn get_input(&mut self) -> Result<u16, DisplayError> {
        let inner = self.inner.lock().unwrap();
        match &inner.fail {
            Some(m) => Err(DisplayError::Backend(m.clone())),
            None => Ok(inner.input),
        }
    }

    fn set_input(&mut self, value: u16) -> Result<(), DisplayError> {
        let mut inner = self.inner.lock().unwrap();
        // Counted before the failure check on purpose. Counting only
        // successes would make "a failed command was still attempted"
        // untestable, and sub-project 5 shipped a test made vacuous by
        // exactly that ordering.
        inner.sets += 1;
        if let Some(m) = &inner.fail {
            return Err(DisplayError::Backend(m.clone()));
        }
        inner.input = value;
        Ok(())
    }

    fn capabilities(&mut self) -> Result<String, DisplayError> {
        let inner = self.inner.lock().unwrap();
        match &inner.fail {
            Some(m) => Err(DisplayError::Backend(m.clone())),
            None => Ok(inner.caps.clone()),
        }
    }
}
```

- [ ] **Step 2: Write the failing tests for `pick`**

Append to `crates/pheme-display/src/lib.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockMonitor;

    fn mon(identity: &str, location: &str) -> Box<dyn Monitor> {
        Box::new(MockMonitor::new(identity, location, 0x11).0)
    }

    /// Break it by returning `Ok` on an empty list: the service then
    /// unwraps a monitor that does not exist.
    #[test]
    fn nothing_enumerated_is_no_monitor() {
        assert!(matches!(
            pick(Vec::new(), None),
            Err(DisplayError::NoMonitor)
        ));
    }

    /// Break it by picking `found.pop()`: the choice becomes the last bus
    /// rather than the first, which reorders every person's monitors.
    #[test]
    fn no_preference_picks_the_first() {
        let found = vec![mon("A", "/dev/i2c-4"), mon("B", "/dev/i2c-9")];
        assert_eq!(pick(found, None).unwrap().identity(), "A");
    }

    /// Break it by comparing with `==` instead of `contains`: a person has
    /// to type the whole identity, serial and all.
    #[test]
    fn a_preference_matches_a_substring_of_the_identity() {
        let found = vec![
            mon("GSM LG ULTRAGEAR (106NTMXE1579)", "/dev/i2c-4"),
            mon("DEL DELL U2720Q (ABC123)", "/dev/i2c-9"),
        ];
        assert_eq!(
            pick(found, Some("u2720")).unwrap().identity(),
            "DEL DELL U2720Q (ABC123)"
        );
    }

    /// Break it by dropping the `location` arm: a monitor whose EDID gave
    /// no name can then never be selected, because its identity is its bus
    /// path and only `location` carries it.
    #[test]
    fn a_preference_also_matches_the_location() {
        let found = vec![mon("A", "/dev/i2c-4"), mon("B", "/dev/i2c-9")];
        assert_eq!(pick(found, Some("i2c-9")).unwrap().identity(), "B");
    }

    /// Break it by falling back to the first monitor: pheme then silently
    /// drives a different screen than the one configured.
    #[test]
    fn a_preference_that_matches_nothing_is_an_error() {
        let found = vec![mon("A", "/dev/i2c-4")];
        assert!(matches!(pick(found, Some("zzz")), Err(DisplayError::NoMatch(w)) if w == "zzz"));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pheme-display`
Expected: FAIL — `cannot find function pick in this scope`.

- [ ] **Step 4: Write `enumerate`, `open` and `pick`**

Add to `crates/pheme-display/src/lib.rs`, above the test module:

```rust
pub mod mock;

#[cfg(target_os = "linux")]
mod backend_i2c;
#[cfg(target_os = "linux")]
use backend_i2c as backend;

#[cfg(windows)]
mod backend_winapi;
#[cfg(windows)]
use backend_winapi as backend;

/// Everywhere else the crate still compiles; the feature is simply never
/// available.
#[cfg(not(any(target_os = "linux", windows)))]
mod backend {
    use super::Monitor;
    pub fn enumerate() -> Vec<Box<dyn Monitor>> {
        Vec::new()
    }
}

/// Every monitor on this machine that answers a read of VCP 0x60.
///
/// Slow: measured at 1.09 s on a two-output Linux laptop, because it opens
/// every i2c bus and reads an EDID from each. Never call it from a thread
/// that carries input.
///
/// Answering the read is the membership test on purpose. A laptop's
/// internal eDP panel enumerates as an i2c bus and returns a valid EDID,
/// but it has no input to select and fails the read, so this keeps it out
/// without special-casing panel types.
pub fn enumerate() -> Vec<Box<dyn Monitor>> {
    backend::enumerate()
}

/// The monitor whose `identity()` or `location()` contains `want`
/// (case-insensitive), or the first one when `want` is `None`.
pub fn open(want: Option<&str>) -> Result<Box<dyn Monitor>, DisplayError> {
    pick(enumerate(), want)
}

/// The choice `open` makes, split out so it is testable without hardware.
fn pick(
    mut found: Vec<Box<dyn Monitor>>,
    want: Option<&str>,
) -> Result<Box<dyn Monitor>, DisplayError> {
    if found.is_empty() {
        return Err(DisplayError::NoMonitor);
    }
    let Some(want) = want else {
        if found.len() > 1 {
            tracing::info!(
                count = found.len(),
                picked = found[0].identity(),
                "more than one monitor answered DDC/CI; set display.monitor to choose"
            );
        }
        return Ok(found.remove(0));
    };
    let needle = want.to_lowercase();
    let idx = found.iter().position(|m| {
        m.identity().to_lowercase().contains(&needle)
            || m.location().to_lowercase().contains(&needle)
    });
    match idx {
        Some(i) => Ok(found.remove(i)),
        None => Err(DisplayError::NoMatch(want.to_string())),
    }
}
```

- [ ] **Step 5: Write the Linux backend**

`crates/pheme-display/src/backend_i2c.rs`:

```rust
//! Linux: DDC/CI over `/dev/i2c-*`.
//!
//! `ddc_i2c::Enumerator` would find the buses, but it is behind the
//! `with-linux-enumerate` feature, which pulls `udev` and `libudev-sys`.
//! Reading the directory costs nothing and keeps a native C dependency out
//! of the build.

use std::path::{Path, PathBuf};

use ddc::{Ddc, Edid};
use ddc_i2c::I2cDeviceDdc;
use tracing::debug;

use crate::{edid::identity_from_edid, DisplayError, Monitor, INPUT_SELECT};

pub struct I2cMonitor {
    ddc: I2cDeviceDdc,
    identity: String,
    location: String,
}

impl Monitor for I2cMonitor {
    fn identity(&self) -> &str {
        &self.identity
    }

    fn location(&self) -> &str {
        &self.location
    }

    fn get_input(&mut self) -> Result<u16, DisplayError> {
        self.ddc
            .get_vcp_feature(INPUT_SELECT)
            .map(|v| v.value())
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn set_input(&mut self, value: u16) -> Result<(), DisplayError> {
        self.ddc
            .set_vcp_feature(INPUT_SELECT, value)
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn capabilities(&mut self) -> Result<String, DisplayError> {
        let raw = self
            .ddc
            .capabilities_string()
            .map_err(|e| DisplayError::Backend(e.to_string()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

pub fn enumerate() -> Vec<Box<dyn Monitor>> {
    // `i2c_paths` returns them ordered by bus number, so the "first
    // monitor" a person gets with no `display.monitor` is the same one on
    // every run. `read_dir` order is not.
    i2c_paths().iter().filter_map(|p| open_bus(p)).collect()
}

fn i2c_paths() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(n) = name.strip_prefix("i2c-") else {
            continue;
        };
        if n.parse::<u32>().is_ok() {
            out.push(e.path());
        }
    }
    // `sort` on the paths would order i2c-10 before i2c-2, so sort on the
    // number the name carries.
    out.sort_by_key(|p| bus_number(p).unwrap_or(u32::MAX));
    out
}

fn bus_number(path: &Path) -> Option<u32> {
    path.file_name()?
        .to_str()?
        .strip_prefix("i2c-")?
        .parse()
        .ok()
}

/// One bus, kept only when something on it answers a read of VCP 0x60.
fn open_bus(path: &Path) -> Option<Box<dyn Monitor>> {
    let mut ddc = match ddc_i2c::from_i2c_device(path) {
        Ok(d) => d,
        Err(e) => {
            debug!(path = %path.display(), error = %e, "opening the i2c device failed");
            return None;
        }
    };
    let location = path.display().to_string();
    let mut raw = vec![0u8; 128];
    let identity = match ddc.read_edid(0, &mut raw) {
        Ok(_) => identity_from_edid(&raw),
        Err(_) => None,
    }
    .unwrap_or_else(|| location.clone());
    if let Err(e) = ddc.get_vcp_feature(INPUT_SELECT) {
        debug!(monitor = %identity, error = %e, "does not answer VCP 0x60; skipping");
        return None;
    }
    Some(Box::new(I2cMonitor {
        ddc,
        identity,
        location,
    }))
}
```

- [ ] **Step 6: Write the Windows backend**

`crates/pheme-display/src/backend_winapi.rs`:

```rust
//! Windows: DDC/CI through Dxva2's `SetVCPFeature`.

use ddc::Ddc;
use ddc_winapi::Monitor as WinMonitor;
use tracing::{debug, warn};

use crate::{DisplayError, Monitor, INPUT_SELECT};

pub struct WinApiMonitor {
    ddc: WinMonitor,
    /// `description()` is all the Win32 API offers, so identity and
    /// location are the same string here.
    description: String,
}

impl Monitor for WinApiMonitor {
    fn identity(&self) -> &str {
        &self.description
    }

    fn location(&self) -> &str {
        &self.description
    }

    fn get_input(&mut self) -> Result<u16, DisplayError> {
        self.ddc
            .get_vcp_feature(INPUT_SELECT)
            .map(|v| v.value())
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn set_input(&mut self, value: u16) -> Result<(), DisplayError> {
        self.ddc
            .set_vcp_feature(INPUT_SELECT, value)
            .map_err(|e| DisplayError::Backend(e.to_string()))
    }

    fn capabilities(&mut self) -> Result<String, DisplayError> {
        let raw = self
            .ddc
            .capabilities_string()
            .map_err(|e| DisplayError::Backend(e.to_string()))?;
        Ok(String::from_utf8_lossy(&raw).into_owned())
    }
}

pub fn enumerate() -> Vec<Box<dyn Monitor>> {
    let monitors = match WinMonitor::enumerate() {
        Ok(m) => m,
        Err(e) => {
            warn!(error = %e, "enumerating physical monitors failed");
            return Vec::new();
        }
    };
    monitors
        .into_iter()
        .filter_map(|mut m| {
            let description = m.description();
            if let Err(e) = m.get_vcp_feature(INPUT_SELECT) {
                debug!(monitor = %description, error = %e, "does not answer VCP 0x60; skipping");
                return None;
            }
            Some(Box::new(WinApiMonitor { ddc: m, description }) as Box<dyn Monitor>)
        })
        .collect()
}
```

- [ ] **Step 7: Add the manual smoke test**

Append to the `tests` module in `crates/pheme-display/src/lib.rs`:

```rust
    /// Prints what this machine's monitors actually are. Ignored because it
    /// needs hardware that answers DDC/CI, which CI does not have and which
    /// the development machine does not either -- `ddcutil detect` reports
    /// "No displays implementing DDC/CI found" there.
    ///
    /// Run with:
    /// `cargo test -p pheme-display -- --ignored --nocapture manual_smoke`
    #[test]
    #[ignore = "needs a monitor that answers DDC/CI"]
    fn manual_smoke() {
        let start = std::time::Instant::now();
        let mut found = enumerate();
        println!("{} monitor(s) in {:?}", found.len(), start.elapsed());
        for m in found.iter_mut() {
            println!("  {} at {}", m.identity(), m.location());
            println!("    input   {:?}", m.get_input());
            println!("    caps    {:?}", m.capabilities());
        }
    }
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p pheme-display`
Expected: PASS, 23 tests, 1 ignored.

- [ ] **Step 9: Check the Windows arm compiles**

```bash
RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone \
  PATH=$HOME/.cargo-standalone/bin:$PATH \
  cargo check -p pheme-display --target x86_64-pc-windows-gnu --all-targets
```
Expected: clean. This step is not optional: `backend_winapi.rs` is compiled by no other command available here, and two Windows-only breakages reached `master` during sub-project 6 for exactly this reason. If the standalone toolchain is missing, say so in the report rather than skipping silently.

- [ ] **Step 10: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 11: Commit**

```bash
git add crates/pheme-display
git commit -F - <<'EOF'
Update: add the DDC/CI backends and monitor selection

The Linux backend reads /dev/i2c-* directly rather than through
ddc_i2c::Enumerator, which lives behind the with-linux-enumerate feature
and pulls udev and libudev-sys. Reading the directory costs nothing.

A bus is kept only when something on it answers a read of VCP 0x60. That
is the membership test rather than a check on connector type because a
laptop's internal eDP panel enumerates as a bus and returns a valid EDID
while having no input to select; failing the read keeps it out on its own.

Buses are ordered by the number in their name, not by read_dir order and
not lexically, so "the first monitor" is the same one on every run and
i2c-10 does not sort before i2c-2.

The Monitor trait is not Send: ddc_winapi::Monitor holds a raw HANDLE with
no unsafe impl Send, so the bound would have failed to compile on Windows
alone. It is not needed -- the handle is created on the service thread and
never leaves it.

pick() carries the choice open() makes so that selection is testable
without hardware; MockMonitor counts attempted sets separately from the
input last set successfully, so a refused command and a failed one cannot
be confused by a test.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 4: Configuration

**Files:**
- Modify: `crates/pheme-app/src/config.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces: `pheme_app::config::DisplayCfg { input: Option<u16>, monitor: Option<String>, cooldown_ms: u64 }`, reachable as `Config::display`; `HotkeysCfg::switch_display: Option<String>`; `Config::hotkeys()` returning a `Hotkeys` that now also carries `switch_display`.

`Config::hotkeys()` cannot fill `switch_display` until Task 6 widens `pheme_core::Hotkeys`. This task adds the config field and its parse; Task 6 adds the core field and wires the two together.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/pheme-app/src/config.rs`:

```rust
    /// Break it by dropping `#[serde(default)]` from `Config::display`: a
    /// config file without a [display] section stops parsing.
    #[test]
    fn a_config_without_a_display_section_leaves_the_feature_off() {
        let c: Config = toml::from_str("name = \"a\"").unwrap();
        assert_eq!(c.display.input, None);
        assert_eq!(c.display.cooldown_ms, 1000);
    }

    /// Break it by typing `input` as a String: TOML's 0x11 then fails to
    /// deserialize and every hex config in the README is rejected.
    #[test]
    fn a_hexadecimal_input_parses_as_a_number() {
        let c: Config = toml::from_str("[display]\ninput = 0x11").unwrap();
        assert_eq!(c.display.input, Some(17));
    }

    /// Break it by dropping `#[serde(default = "default_cooldown_ms")]`:
    /// a [display] section naming only `input` gets a zero cooldown, and
    /// every crossing commands the monitor mid-switch.
    #[test]
    fn a_display_section_without_a_cooldown_gets_the_default() {
        let c: Config = toml::from_str("[display]\ninput = 15").unwrap();
        assert_eq!(c.display.cooldown_ms, 1000);
    }

    /// Break it by dropping `#[serde(default)]` from
    /// `HotkeysCfg::switch_display`: every existing config file that names
    /// only `lock` stops parsing, because HotkeysCfg is
    /// deny_unknown_fields with a hand-written Default.
    #[test]
    fn a_hotkeys_section_naming_only_lock_still_parses() {
        let c: Config = toml::from_str("[hotkeys]\nlock = \"ScrollLock\"").unwrap();
        assert_eq!(c.hotkeys.lock.as_deref(), Some("ScrollLock"));
        assert_eq!(c.hotkeys.switch_display, None);
    }

    /// Break it by giving `switch_display` a non-None default: a person who
    /// never asked for the feature gets a key of theirs swallowed by it.
    #[test]
    fn switch_display_has_no_default_key() {
        assert_eq!(HotkeysCfg::default().switch_display, None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib config`
Expected: FAIL — `no field display on type Config`.

- [ ] **Step 3: Add `DisplayCfg`**

In `crates/pheme-app/src/config.rs`, beside `AudioCfg`:

```rust
/// Monitor input switching (sub-project 7). Off unless `input` is set:
/// there is no sensible default for a number that names a physical cable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DisplayCfg {
    /// The VCP 0x60 value of the input *this* machine is cabled to.
    ///
    /// Vendor-specific: MCCS assigns 0x0F to DisplayPort-1 and 0x11 to
    /// HDMI-1 and vendors disregard it freely, which is what `pheme
    /// displays` is for. TOML accepts `0x11`; `toml::to_string_pretty`
    /// writes it back as `17`, which is the same number.
    pub input: Option<u16>,
    /// Which monitor, when more than one answers. A case-insensitive
    /// substring of the identity or location `pheme displays` prints.
    pub monitor: Option<String>,
    /// Minimum gap between commands. Monitors take one to three seconds to
    /// switch and re-sync, and a command arriving mid-switch is at best
    /// ignored.
    pub cooldown_ms: u64,
}

impl Default for DisplayCfg {
    fn default() -> Self {
        DisplayCfg {
            input: None,
            monitor: None,
            cooldown_ms: default_cooldown_ms(),
        }
    }
}

fn default_cooldown_ms() -> u64 {
    1000
}
```

Add the field to `Config`, after `audio`:

```rust
    #[serde(default)]
    pub display: DisplayCfg,
```

and to `Config::default()`, after `audio: AudioCfg::default(),`:

```rust
            display: DisplayCfg::default(),
```

- [ ] **Step 4: Add the hotkey field**

In `HotkeysCfg`:

```rust
    /// Re-assert the monitor's input for whichever machine holds the
    /// pointer (design section 8). `#[serde(default)]` is load-bearing:
    /// `HotkeysCfg` is `deny_unknown_fields` with a hand-written `Default`,
    /// so without it every existing `[hotkeys]` section naming only `lock`
    /// would stop parsing.
    #[serde(default)]
    pub switch_display: Option<String>,
```

and in `impl Default for HotkeysCfg`, beside `lock`:

```rust
            switch_display: None,
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --lib config`
Expected: PASS.

- [ ] **Step 6: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-app/src/config.rs
git commit -F - <<'EOF'
Update: add the display section to the configuration

Each machine declares the VCP 0x60 value of the input it is itself cabled
to, which is a question a person can answer from their monitor's on-screen
menu, and which stays one number per machine when a server has several
clients configured. The peer's value travels in the handshake.

The section is absent by default and absent means off. There is no
sensible default for a number that names a physical cable, and guessing
one would have pheme command a monitor nobody asked it to touch.

hotkeys.switch_display carries #[serde(default)] because HotkeysCfg is
deny_unknown_fields with a hand-written Default: without it, every
existing config file naming only hotkeys.lock would stop parsing.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 5: Protocol

**Files:**
- Modify: `crates/pheme-proto/src/lib.rs`
- Modify: `crates/pheme-app/src/server.rs` (the `Hello` destructure and the `HelloAck` construction)
- Modify: `crates/pheme-app/src/client.rs` (the `Hello` construction and the `HelloAck` destructure)
- Modify: `crates/pheme-app/tests/mic_e2e.rs`, `crates/pheme-net/tests/transport.rs` (construction sites)

**Interfaces:**
- Consumes: nothing.
- Produces: `Msg::Hello { version, name, os, screens, audio, display_input: Option<u16> }`, `Msg::HelloAck { version, name, audio, display_input: Option<u16> }`, `Msg::SwitchDisplay { input: u16 }`, `PROTOCOL_VERSION == 3`.

This task deliberately passes `None` at every construction site and ignores the received value. Tasks 9 and 10 fill them in. Keeping the wire change in its own commit means a bisect lands on "the protocol changed" separately from "the server started using it".

- [ ] **Step 1: Write the failing tests**

In `crates/pheme-proto/src/lib.rs`, update the existing version guard and add round-trip tests beside the other message tests:

```rust
    /// The guard that makes a wire change deliberate. postcard is not
    /// self-describing, so an added field is a wire change: both ends
    /// compare versions for equality and refuse a mismatch, which is the
    /// correct outcome and means both machines upgrade together.
    #[test]
    fn protocol_version_is_pinned() {
        assert_eq!(PROTOCOL_VERSION, 3);
    }

    /// Break it by typing `display_input` as `u16` rather than
    /// `Option<u16>`: a peer with no monitor has no value to send, and 0 is
    /// a legal VCP input value on some hardware.
    #[test]
    fn hello_carries_an_optional_display_input() {
        for v in [None, Some(0x11u16)] {
            let m = Msg::Hello {
                version: PROTOCOL_VERSION,
                name: "a".into(),
                os: Os::Linux,
                screens: Vec::new(),
                audio: AudioParams::DEFAULT,
                display_input: v,
            };
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<Msg>(&bytes).unwrap(), m);
        }
    }

    #[test]
    fn hello_ack_carries_an_optional_display_input() {
        for v in [None, Some(0x0fu16)] {
            let m = Msg::HelloAck {
                version: PROTOCOL_VERSION,
                name: "s".into(),
                audio: AudioParams::DEFAULT,
                display_input: v,
            };
            let bytes = postcard::to_stdvec(&m).unwrap();
            assert_eq!(postcard::from_bytes::<Msg>(&bytes).unwrap(), m);
        }
    }

    #[test]
    fn switch_display_round_trips() {
        let m = Msg::SwitchDisplay { input: 0x12 };
        let bytes = postcard::to_stdvec(&m).unwrap();
        assert_eq!(postcard::from_bytes::<Msg>(&bytes).unwrap(), m);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-proto`
Expected: FAIL — `assertion left == right failed: 2 vs 3`, and `struct variant Msg::Hello has no field named display_input`.

- [ ] **Step 3: Change the protocol**

In `crates/pheme-proto/src/lib.rs`:

```rust
pub const PROTOCOL_VERSION: u16 = 3;
```

Add the field to `Hello`, after `audio`:

```rust
    /// The VCP 0x60 value of the monitor input this peer is cabled to, or
    /// `None` when it has no monitor control configured. The peer switching
    /// away from itself needs the *other* machine's value, so each end
    /// declares its own here (sub-project 7 design section 6).
    pub display_input: Option<u16>,
```

the same field, with the same comment, to `HelloAck` after `audio`, and this variant appended after the last existing one (`Clipboard`), so no existing variant index moves:

```rust
    /// Either direction: "if you are the input the monitor is showing,
    /// select `input`".
    ///
    /// Sent by the recovery hotkey only, never on a crossing. DDC/CI is
    /// answered only by the input currently displayed, so the machine that
    /// wants the screen back cannot command the monitor itself; it asks the
    /// machine that is on screen to do it, and both try.
    SwitchDisplay {
        input: u16,
    },
```

- [ ] **Step 4: Fix every construction site**

`crates/pheme-app/src/client.rs`, the `Msg::Hello` it sends — add `display_input: None,` after `audio: AudioParams::DEFAULT,`. Its `Msg::HelloAck` destructure gains `display_input: _,`.

`crates/pheme-app/src/server.rs`, the `Msg::Hello` destructure gains `display_input: _,`; the `Msg::HelloAck` it sends gains `display_input: None,`.

`crates/pheme-app/tests/mic_e2e.rs` and `crates/pheme-net/tests/transport.rs`: add `display_input: None,` to each `Msg::Hello`/`Msg::HelloAck` they build.

Find them all with:

```bash
rg -n 'Msg::(Hello|HelloAck)' crates/
```

- [ ] **Step 5: Handle the new variant where `Msg` is matched exhaustively**

```bash
cargo check --workspace --all-targets 2>&1 | grep -A5 'non-exhaustive'
```

`pheme-core`'s client (`crates/pheme-core/src/client.rs:47`) matches on `Msg`; add `Msg::SwitchDisplay { .. }` to whatever arm ignores messages the core does not act on, because the core is not where this is handled. `crates/pheme-app/src/client.rs:152`'s `input_seq` must return `None` for it — it carries no sequence number.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 8: Commit**

```bash
git add crates/pheme-proto crates/pheme-app crates/pheme-net
git commit -F - <<'EOF'
Update: carry each peer's monitor input in the handshake

Hello and HelloAck gain display_input, and a SwitchDisplay message is
appended for the recovery hotkey. Nothing reads either yet; this keeps
the wire change on its own commit so a bisect separates "the protocol
changed" from "the server started using it".

Each end declares the input it is itself cabled to rather than the one it
wants the peer to select. DDC/CI is answered only by the input currently
displayed, so the machine handing the pointer over is the one that must
issue the command, and it needs the other machine's value to do it.

PROTOCOL_VERSION goes to 3. postcard is not self-describing, so an added
field is a wire change; both ends already compare versions for equality
and refuse a mismatch, which is correct and means both machines upgrade
together. The guard test asserting the version is updated in the same
commit, which is what it exists for.

SwitchDisplay is appended after Clipboard so no existing variant index
moves.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 6: The core hotkey

**Files:**
- Modify: `crates/pheme-core/src/server.rs`
- Modify: `crates/pheme-app/src/config.rs` (fill `switch_display` in `Config::hotkeys()`)
- Modify: `crates/pheme-app/src/server.rs` (exhaustive match on `Action`)

**Interfaces:**
- Consumes: `HotkeysCfg::switch_display` (Task 4).
- Produces: `pheme_core::server::Hotkeys { lock: Option<KeyCode>, switch_display: Option<KeyCode> }` and `pheme_core::server::Action::SwitchDisplay { local: bool }`.

`local` is `self.remote.is_none()` at the moment the key went down: the core says which machine holds the pointer, and never learns what a VCP value is.

- [ ] **Step 1: Write the failing tests**

In the `tests` module of `crates/pheme-core/src/server.rs`. Note the first
change: the existing `core()` helper builds `Hotkeys { lock: Some(LOCK) }`
and stops compiling the moment the struct gains a field, so widen it in the
same edit.

```rust
    const SWITCH: KeyCode = KeyCode(0x45); // F12

    /// `core`, with a switch-display hotkey bound as well.
    fn core_with_switch(side: Side, span: (f32, f32)) -> ServerCore {
        let layout = Layout {
            server_screens: screen(1920, 1080),
            clients: vec![ClientPlacement {
                name: "lap".into(),
                side,
                span,
            }],
        };
        let mut c = ServerCore::new(
            layout,
            Hotkeys {
                lock: Some(LOCK),
                switch_display: Some(SWITCH),
            },
        );
        c.client_connected("lap", screen(1000, 500));
        c
    }

    /// Break it by hard-coding `local: true`: the hotkey then always asks
    /// for the server's input, so it can never bring the screen back from a
    /// client -- the one case it exists for.
    #[test]
    fn the_switch_display_hotkey_reports_which_machine_holds_the_pointer() {
        let mut c = core_with_switch(Side::Right, (0.0, 1.0));
        let a = c.on_event(CaptureEvent::Key {
            code: SWITCH,
            down: true,
        });
        assert_eq!(a, vec![Action::SwitchDisplay { local: true }]);

        enter_right(&mut c);
        assert!(matches!(c.active(), Active::Remote(_)));

        let a = c.on_event(CaptureEvent::Key {
            code: SWITCH,
            down: true,
        });
        assert_eq!(a, vec![Action::SwitchDisplay { local: false }]);
    }

    /// Break it by testing the hotkey after the local/remote split rather
    /// than before it: the key is then forwarded to the client, typed into
    /// whatever has focus there, and the person who cannot see their screen
    /// has no way back.
    #[test]
    fn the_switch_display_hotkey_is_never_forwarded() {
        let mut c = core_with_switch(Side::Right, (0.0, 1.0));
        enter_right(&mut c);
        let a = c.on_event(CaptureEvent::Key {
            code: SWITCH,
            down: true,
        });
        assert!(
            !a.iter()
                .any(|x| matches!(x, Action::SendControl(Msg::Key { .. }))),
            "the hotkey reached the client: {a:?}"
        );
    }

    /// Break it by acting on key-up as well: one press then commands the
    /// monitor twice, and the second command lands mid-switch.
    #[test]
    fn the_switch_display_hotkey_acts_on_the_way_down_only() {
        let mut c = core_with_switch(Side::Right, (0.0, 1.0));
        let a = c.on_event(CaptureEvent::Key {
            code: SWITCH,
            down: false,
        });
        assert!(a.is_empty(), "{a:?}");
    }
```

Widen the existing helper in the same edit:

```rust
        let mut c = ServerCore::new(
            layout,
            Hotkeys {
                lock: Some(LOCK),
                switch_display: None,
            },
        );
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-core`
Expected: FAIL — `struct Hotkeys has no field named switch_display`.

- [ ] **Step 3: Widen `Hotkeys` and `Action`**

In `crates/pheme-core/src/server.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hotkeys {
    pub lock: Option<KeyCode>,
    /// Re-assert the monitor's input for whichever machine holds the
    /// pointer. Sub-project 7 design section 8.
    pub switch_display: Option<KeyCode>,
}
```

and, in `Action`:

```rust
    /// Re-assert the monitor input for the machine that holds the pointer.
    ///
    /// `local` says which machine that is; it is all the core knows, and
    /// deliberately so -- a VCP value names a physical cable and belongs to
    /// the configuration, not to the state machine. The app turns this into
    /// its own `display.input` or the peer's.
    SwitchDisplay {
        local: bool,
    },
```

- [ ] **Step 4: Handle the key**

In `ServerCore::on_event`, inside the existing `if let CaptureEvent::Key { code, down } = ev` block, directly after the `hotkeys.lock` arm and before `self.held` is touched:

```rust
            if Some(code) == self.hotkeys.switch_display {
                if down {
                    return vec![Action::SwitchDisplay {
                        local: self.remote.is_none(),
                    }];
                }
                return Vec::new();
            }
```

Placing it here, above the local/remote split, is the whole point: the key is intercepted in both states and never forwarded. A person who cannot see their screen still reaches it.

- [ ] **Step 5: Fill the config field**

In `crates/pheme-app/src/config.rs`, `Config::hotkeys()`, before `Ok(Hotkeys { lock })`:

```rust
        // No portal-trigger branch here: unlike `lock`, this hotkey is never
        // bound through the GlobalShortcuts portal, so a value containing
        // `+` names no key and is an error on every platform.
        let switch_display = match self.hotkeys.switch_display.as_deref() {
            None | Some("") => None,
            Some(name) => match key_by_name(name) {
                Some(code) => Some(code),
                None => bail!("unknown key name for hotkeys.switch_display: {name:?}"),
            },
        };
        Ok(Hotkeys {
            lock,
            switch_display,
        })
```

and add a test beside the other hotkey tests:

```rust
    #[test]
    fn an_unknown_switch_display_key_is_an_error() {
        let c: Config = toml::from_str("[hotkeys]\nswitch_display = \"NoSuchKey\"").unwrap();
        assert!(c.hotkeys().is_err());
    }
```

- [ ] **Step 6: Make the app's `Action` match exhaustive**

`crates/pheme-app/src/server.rs`, `Shared::run_actions`, add an arm that does nothing yet — Task 9 fills it:

```rust
                // Task 8 wires this to DisplayService and the peer.
                Action::SwitchDisplay { .. } => {}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p pheme-core -p pheme-app`
Expected: PASS.

- [ ] **Step 8: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 9: Commit**

```bash
git add crates/pheme-core crates/pheme-app
git commit -F - <<'EOF'
Update: add the switch-display hotkey to the core state machine

The key is tested above the local/remote split, beside the lock hotkey,
so the server intercepts it whether the pointer is local or remote and
never forwards it to the client. That placement is the point of the
hotkey: it is the recovery path for a monitor showing the wrong machine,
and the person using it cannot see the screen they would otherwise need.

Action::SwitchDisplay carries only whether the pointer is local. A VCP
value names a physical cable and belongs to the configuration, so the
core reports which machine holds the pointer and the app decides which
number that means.

Acting on key-down alone keeps one press from commanding the monitor
twice, where the second command would arrive mid-switch and be ignored.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 7: `DisplayService`, the thread that owns the monitor

**Files:**
- Create: `crates/pheme-app/src/display.rs`
- Create: `crates/pheme-app/tests/display.rs`
- Modify: `crates/pheme-app/src/lib.rs` (`pub mod display;`)
- Modify: `crates/pheme-app/Cargo.toml` (`pheme-display = { workspace = true }`)

**Interfaces:**
- Consumes: `pheme_display::{DisplaySwitch, DisplayError, Monitor}` (Tasks 1, 3), `DisplayCfg` (Task 4).
- Produces: `pheme_app::display::{DisplayService, OpenFn}`; `DisplayService::spawn(&DisplayCfg, OpenFn) -> Option<DisplayService>`, `switch_to(u16)`, `force(u16)`.

- [ ] **Step 1: Add the dependency**

In `crates/pheme-app/Cargo.toml`, beside `pheme-clip`:

```toml
pheme-display = { workspace = true }
```

- [ ] **Step 2: Write the failing tests**

`crates/pheme-app/tests/display.rs`:

```rust
//! `DisplayService` driven end to end over a `MockMonitor`.
//!
//! The service runs on its own thread, so every assertion waits for the
//! effect rather than assuming it has happened. Asserting immediately would
//! produce a test that passes or fails on timing, which is worse than one
//! that fails honestly.

use std::time::{Duration, Instant};

use pheme_app::config::DisplayCfg;
use pheme_app::display::DisplayService;
use pheme_display::mock::{MockMonitor, MockMonitorHandle};
use pheme_display::DisplayError;

fn cfg(input: u16, cooldown_ms: u64) -> DisplayCfg {
    DisplayCfg {
        input: Some(input),
        monitor: None,
        cooldown_ms,
    }
}

/// A service over a mock that starts on `start_input`.
fn service(cooldown_ms: u64, start_input: u16) -> (DisplayService, MockMonitorHandle) {
    let (mon, handle) = MockMonitor::new("MOCK", "mock", start_input);
    let svc = DisplayService::spawn(&cfg(0x11, cooldown_ms), Box::new(move || Ok(Box::new(mon))))
        .expect("the feature is configured on");
    (svc, handle)
}

fn wait_for(what: &str, f: impl Fn() -> bool) {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("{what} did not happen within 5 s");
}

/// Break it by removing the `input.is_none()` guard from `spawn`: pheme
/// then starts a thread and enumerates monitors for every person who never
/// asked for this feature, costing a second of startup each time.
#[test]
fn an_unconfigured_input_means_no_service() {
    let (mon, _handle) = MockMonitor::new("MOCK", "mock", 0x11);
    let off = DisplayCfg {
        input: None,
        monitor: None,
        cooldown_ms: 1000,
    };
    assert!(DisplayService::spawn(&off, Box::new(move || Ok(Box::new(mon)))).is_none());
}

/// Review Focus 4, in its local form: no monitor to talk to. Break it by
/// unwrapping the result of the `OpenFn`: the thread panics, and on a
/// machine whose monitor ignores DDC/CI -- a common case -- that panic
/// happens on every start.
#[test]
fn a_failing_open_leaves_a_service_that_does_nothing() {
    let svc = DisplayService::spawn(
        &cfg(0x11, 1000),
        Box::new(|| Err(DisplayError::NoMonitor)),
    )
    .expect("the feature is configured on");
    for _ in 0..10 {
        svc.switch_to(0x0f);
        svc.force(0x0f);
    }
    // Nothing to assert on but survival: the point is that none of those
    // calls panicked and none of them blocked.
}

/// Break it by having the thread ignore `Req::Switch`: nothing ever
/// reaches the monitor.
#[test]
fn a_switch_reaches_the_monitor() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the input to change", || handle.input() == 0x0f);
}

/// Break it by calling `policy.confirm` instead of `policy.forget` on a
/// failed `set_input`: the retry is then refused by rule 1 and the monitor
/// stays wrong forever. This is the `ClipSync` defect from sub-project 5,
/// pinned end to end rather than on the policy alone.
#[test]
fn a_failed_command_does_not_refuse_the_retry() {
    let (svc, handle) = service(0, 0x11);
    handle.fail_with("i2c timeout");
    svc.switch_to(0x0f);
    wait_for("the failing attempt", || handle.sets() == 1);
    assert_eq!(handle.input(), 0x11, "a failed set must not change the input");

    handle.stop_failing();
    svc.switch_to(0x0f);
    wait_for("the retry to land", || handle.input() == 0x0f);
    assert_eq!(handle.sets(), 2);
}

/// Break it by routing `force` through `policy.request`: the deduplicated
/// value is refused and the recovery path does nothing at all.
#[test]
fn force_reaches_the_monitor_where_a_switch_would_be_deduplicated() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    let after_first = handle.sets();

    // Deduplicated: the monitor is already on 0x0f.
    svc.switch_to(0x0f);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(handle.sets(), after_first, "a repeat should not be commanded");

    svc.force(0x0f);
    wait_for("the forced command", || handle.sets() == after_first + 1);
}

/// Review Focus 2. Break it by treating a zero cooldown as "always in
/// cooldown": the second value is held behind a deadline that has already
/// passed, and the loop re-arms it forever without ever issuing it.
#[test]
fn a_zero_cooldown_switches_every_time() {
    let (svc, handle) = service(0, 0x11);
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    svc.switch_to(0x12);
    wait_for("the second switch", || handle.input() == 0x12);
}

/// Break it by storing the value before `set_input` rather than after it:
/// the status line then reports an input the monitor refused, which is
/// exactly the reading a person uses to tell "switched" from "ignored".
#[test]
fn only_a_confirmed_switch_is_reported() {
    let (svc, handle) = service(0, 0x11);
    assert_eq!(svc.last_input(), None);

    handle.fail_with("i2c timeout");
    svc.switch_to(0x0f);
    wait_for("the failing attempt", || handle.sets() == 1);
    assert_eq!(svc.last_input(), None, "a refused command must not be reported");

    handle.stop_failing();
    svc.switch_to(0x0f);
    wait_for("the retry to land", || handle.input() == 0x0f);
    wait_for("the report", || svc.last_input() == Some(0x0f));
}

/// Rule 3 through the real thread: a value asked for during the cooldown
/// is issued when the cooldown ends, not dropped. Break it by dropping the
/// held request in the service loop, or by passing `rx.recv()` where the
/// deadline arm belongs: the second value never arrives.
#[test]
fn a_request_made_during_the_cooldown_still_arrives() {
    let (svc, handle) = service(200, 0x11);
    svc.switch_to(0x0f);
    wait_for("the first switch", || handle.input() == 0x0f);
    svc.switch_to(0x12);
    wait_for("the held switch", || handle.input() == 0x12);
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --test display`
Expected: FAIL — `unresolved import pheme_app::display`.

- [ ] **Step 4: Write the service**

`crates/pheme-app/src/display.rs`:

```rust
//! The thread that owns the monitor handle.
//!
//! Everything slow about DDC/CI lives here. Enumeration costs about a
//! second and a write costs hundreds of milliseconds, so the router thread
//! only ever does a non-blocking `try_send` into this channel and walks
//! away.

use std::time::{Duration, Instant};

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError};
use pheme_display::{DisplayError, DisplaySwitch, Monitor};
use tracing::{debug, info, warn};

use crate::config::DisplayCfg;

/// How the service acquires its monitor.
///
/// Injected so the tests can hand it a `MockMonitor`; production passes a
/// closure that has already captured `display.monitor`. A closure's `Send`
/// depends on what it captures, not on what it returns, so this stays
/// `Send` even though `Monitor` is not.
pub type OpenFn = Box<dyn FnOnce() -> Result<Box<dyn Monitor>, DisplayError> + Send>;

enum Req {
    Switch(u16),
    Force(u16),
}

/// Four, the same depth the status channel uses. A full queue means four
/// crossings are already waiting on a monitor that takes seconds to answer,
/// and the oldest of them is no longer anyone's intention.
const QUEUE: usize = 4;

pub struct DisplayService {
    tx: Sender<Req>,
    /// The input last *successfully* commanded, for the front-end's status
    /// line. `u32::MAX` means "nothing yet", so one atomic carries both
    /// states and the status path takes no lock at all -- it is read once a
    /// second from the thread that also carries input.
    last: Arc<AtomicU32>,
}

/// `last`'s sentinel for "no input has been confirmed".
const NO_INPUT: u32 = u32::MAX;

impl DisplayService {
    /// `None` when the feature is off, that is when `[display] input` is
    /// unset.
    ///
    /// This returns before the monitor is opened, so it cannot and does not
    /// report whether one answered: enumeration costs about a second and
    /// runs on the spawned thread. A thread that finds no monitor logs once
    /// and exits, which disconnects the channel and turns every later
    /// `switch_to` into a dropped send.
    pub fn spawn(cfg: &DisplayCfg, open: OpenFn) -> Option<DisplayService> {
        if cfg.input.is_none() {
            return None;
        }
        let cooldown = Duration::from_millis(cfg.cooldown_ms);
        let (tx, rx) = crossbeam_channel::bounded(QUEUE);
        let last = Arc::new(AtomicU32::new(NO_INPUT));
        let thread_last = Arc::clone(&last);
        if let Err(e) = std::thread::Builder::new()
            .name("pheme-display".into())
            .spawn(move || run(rx, open, cooldown, thread_last))
        {
            warn!(error = %e, "could not start the display thread; switching is off");
            return None;
        }
        Some(DisplayService { tx, last })
    }

    /// The input this machine last successfully commanded, or `None` when
    /// nothing has been. Read by the status path once a second.
    pub fn last_input(&self) -> Option<u16> {
        match self.last.load(Ordering::Relaxed) {
            NO_INPUT => None,
            v => Some(v as u16),
        }
    }

    /// A pointer crossing asks for `value`. Never blocks and never fails.
    pub fn switch_to(&self, value: u16) {
        self.send(Req::Switch(value));
    }

    /// The recovery hotkey asks for `value`, bypassing the policy's dedupe
    /// and cooldown. Never blocks and never fails.
    pub fn force(&self, value: u16) {
        self.send(Req::Force(value));
    }

    fn send(&self, req: Req) {
        match self.tx.try_send(req) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                debug!("the display queue is full; dropping a switch")
            }
            // The thread found no monitor and exited. Ordinary, and already
            // logged once by the thread itself.
            Err(TrySendError::Disconnected(_)) => {}
        }
    }
}

fn run(rx: Receiver<Req>, open: OpenFn, cooldown: Duration, last: Arc<AtomicU32>) {
    let mut mon = match open() {
        Ok(m) => m,
        Err(e) => {
            // Once, and never retried on any schedule. A machine whose
            // monitor does not speak DDC/CI is an ordinary machine, not a
            // fault worth repeating.
            warn!(error = %e, "display switching is off: no usable monitor");
            return;
        }
    };
    info!(monitor = mon.identity(), at = mon.location(), "display switching is on");
    let mut policy = DisplaySwitch::new(cooldown);
    match mon.get_input() {
        Ok(v) => policy.observe(v),
        Err(e) => debug!(
            error = %e,
            "could not read the current input; the first switch will be issued blind"
        ),
    }
    let mut warned = false;
    loop {
        // `None` here means the deadline fired rather than a request
        // arriving: a request held by the cooldown needs a wakeup that no
        // incoming message will provide. With nothing held, `deadline` is
        // `None` and this blocks indefinitely -- which is also what makes
        // `cooldown_ms = 0` cost no spinning, since nothing is ever held.
        let req = match policy.deadline() {
            Some(t) => match rx.recv_deadline(t) {
                Ok(r) => Some(r),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(r) => Some(r),
                Err(_) => break,
            },
        };
        let now = Instant::now();
        let issue = match req {
            Some(Req::Switch(v)) => policy.request(v, now),
            Some(Req::Force(v)) => Some(policy.force(v, now)),
            None => policy.poll(now),
        };
        if let Some(v) = issue {
            apply(mon.as_mut(), &mut policy, v, &mut warned, &last);
        }
    }
}

fn apply(
    mon: &mut dyn Monitor,
    policy: &mut DisplaySwitch,
    value: u16,
    warned: &mut bool,
    last: &AtomicU32,
) {
    match mon.set_input(value) {
        Ok(()) => {
            policy.confirm(value);
            last.store(u32::from(value), Ordering::Relaxed);
            debug!(input = value, "monitor input switched");
        }
        Err(e) => {
            // `forget`, never `confirm`. Recording a value the command did
            // not deliver would leave the policy believing the monitor shows
            // something it does not, and rule 1 would then refuse every
            // attempt to correct it.
            policy.forget();
            if *warned {
                debug!(input = value, error = %e, "setting the monitor input failed");
            } else {
                *warned = true;
                warn!(
                    input = value,
                    error = %e,
                    "setting the monitor input failed; further failures log at debug"
                );
            }
        }
    }
}
```

- [ ] **Step 5: Declare the module**

In `crates/pheme-app/src/lib.rs`, beside `pub mod clipboard;`:

```rust
pub mod display;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p pheme-app --test display`
Expected: PASS, 8 tests.

- [ ] **Step 7: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 8: Commit**

```bash
git add crates/pheme-app/Cargo.toml Cargo.lock crates/pheme-app/src/display.rs \
        crates/pheme-app/src/lib.rs crates/pheme-app/tests/display.rs
git commit -F - <<'EOF'
Update: run monitor switching on its own thread

Everything slow about DDC/CI lives behind one bounded channel. Opening a
monitor costs about a second on a two-output laptop and a write costs
hundreds of milliseconds, so the router thread does a non-blocking
try_send and walks away; a full queue drops the oldest intention, which
by then is four crossings out of date.

spawn returns before the monitor is opened and so cannot report whether
one answered. A thread that finds none warns once and exits, which
disconnects the channel and makes every later switch_to a dropped send.
It never retries on any schedule: a machine whose monitor ignores DDC/CI
is ordinary, not a fault worth repeating every time somebody crosses the
screen edge.

The loop waits on a deadline rather than a timeout so a request held by
the cooldown gets the wakeup no incoming message would provide. With
nothing held there is no deadline and the loop blocks, which is what
keeps cooldown_ms = 0 from spinning.

A failed set_input calls forget, not confirm. Recording a value the
command did not deliver would leave the policy believing the monitor
shows something it does not, and its first rule would then refuse every
attempt to correct it.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 8: Server wiring

**Files:**
- Modify: `crates/pheme-app/src/server.rs`

**Interfaces:**
- Consumes: `DisplayService` (Task 7), `Action::SwitchDisplay { local }` (Task 6), `Msg::{Hello, HelloAck, SwitchDisplay}` and `display_input` (Task 5), `DisplayCfg` (Task 4).
- Produces: nothing later tasks consume.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `crates/pheme-app/src/server.rs`:

```rust
    /// Review Focus 1: the likeliest misconfiguration. Both machines
    /// declare the same input, so every crossing commands the monitor to
    /// the cable it is already on and nothing ever visibly happens. Break
    /// it by deleting the comparison: the person gets no diagnostic at all
    /// and no way to discover why the feature does nothing.
    #[test]
    fn matching_display_inputs_are_reported() {
        assert!(display_input_conflict(Some(0x11), Some(0x11)));
        assert!(!display_input_conflict(Some(0x11), Some(0x0f)));
        assert!(!display_input_conflict(None, Some(0x11)));
        assert!(!display_input_conflict(Some(0x11), None));
        assert!(!display_input_conflict(None, None));
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p pheme-app --lib server`
Expected: FAIL — `cannot find function display_input_conflict`.

- [ ] **Step 3: Add the conflict check**

In `crates/pheme-app/src/server.rs`, near the handshake code:

```rust
/// Whether both machines claim the same monitor input.
///
/// A crossing then commands the monitor to the cable it is already on, so
/// the picture never moves and nothing says why. It is the likeliest way
/// to misconfigure this feature, because both ends are configured from the
/// same instructions and the value is easy to copy across.
fn display_input_conflict(ours: Option<u16>, theirs: Option<u16>) -> bool {
    matches!((ours, theirs), (Some(a), Some(b)) if a == b)
}
```

- [ ] **Step 4: Carry the client's input on the link**

Add to `struct Link` (`crates/pheme-app/src/server.rs:77`), after `name`:

```rust
    /// The VCP 0x60 value the client says it is cabled to, from its
    /// `Hello`. Kept here rather than in a `Mutex` on `Shared` because
    /// `run_actions` already clones the link at its top, and a disconnect
    /// clears this for free by replacing the whole `Link`.
    display_input: Option<u16>,
```

Fill it where the `Link` is built, from the `Hello` that Task 5 made carry it, and change the server's `Hello` destructure from `display_input: _,` to `display_input: client_display_input,`. Where the `HelloAck` is sent, replace `display_input: None,` with `display_input: cfg_display_input,` — the server's own `display.input`, threaded into this function the way `server_name` already is (add a field to `ServerDeps` and to whatever struct the handshake function receives; follow how `lock_hotkey_trigger` is threaded).

Immediately after the version check passes, add:

```rust
    if display_input_conflict(cfg_display_input, client_display_input) {
        warn!(
            input = ?cfg_display_input,
            client = %name,
            "this machine and the client claim the same monitor input; crossing the \
             edge will command the monitor to the cable it is already on. Set \
             display.input on each machine to the input that machine is cabled to."
        );
    }
```

- [ ] **Step 5: Spawn the service and hook the crossing**

Add to `struct Shared`, beside `clipboard`:

```rust
    /// The monitor worker, or `None` when `[display] input` is unset.
    /// `None` disables input switching and nothing else.
    pub display: Option<DisplayService>,
```

Create it beside the clipboard at `crates/pheme-app/src/server.rs:935`:

```rust
    let display = {
        let monitor = cfg.display.monitor.clone();
        DisplayService::spawn(
            &cfg.display,
            Box::new(move || pheme_display::open(monitor.as_deref())),
        )
    };
```

and pass it through `ServerDeps` the way `clipboard` is passed.

In `Shared::run_actions`, in the `Action::SendControl(m)` arm, extend the existing `Msg::Enter` block:

```rust
                        if matches!(m, Msg::Enter { .. }) {
                            if let Some(c) = &self.clipboard {
                                c.send_to(l.sender.clone());
                            }
                            // The picture crosses with the pointer. The
                            // server is the displayed input right now, which
                            // is the only moment its DDC/CI command can
                            // reach the monitor.
                            if let (Some(d), Some(v)) = (&self.display, l.display_input) {
                                d.switch_to(v);
                            }
                        }
```

- [ ] **Step 6: Handle the hotkey action**

Replace the placeholder arm from Task 6:

```rust
                Action::SwitchDisplay { local } => {
                    // The target is the input of whichever machine holds the
                    // pointer. Both machines are asked for it: DDC/CI is
                    // answered only by the input currently displayed, so the
                    // one that is on screen succeeds and the other fails
                    // harmlessly. Without this the hotkey could never bring
                    // the screen back from a client, which is the case it
                    // exists for.
                    let target = if local {
                        self.display_input
                    } else {
                        link.as_ref().and_then(|l| l.display_input)
                    };
                    if let Some(v) = target {
                        if let Some(d) = &self.display {
                            d.force(v);
                        }
                        if let Some(l) = &link {
                            let _ = l.control.send(Msg::SwitchDisplay { input: v });
                        }
                    }
                }
```

`Shared` gains `pub display_input: Option<u16>` holding this machine's own `cfg.display.input`.

- [ ] **Step 7: Handle an incoming `SwitchDisplay`**

Wherever the server dispatches messages received from the client, add:

```rust
            Msg::SwitchDisplay { input } => {
                // Review Focus 4: a peer with a monitor can send this to a
                // peer without one. `self.display` is `None` there and this
                // is a no-op, which is the whole handling.
                if let Some(d) = &shared.display {
                    d.force(input);
                }
            }
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p pheme-app`
Expected: PASS.

- [ ] **Step 9: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 10: Commit**

```bash
git add crates/pheme-app/src/server.rs
git commit -F - <<'EOF'
Update: switch the monitor when the server hands the pointer over

The hook sits beside the clipboard's, in the Msg::Enter branch, because
both cross for the same reason and at the same moment. That moment is the
only one at which the server's command can reach the monitor: DDC/CI is
answered by the displayed input alone, and the server is still it.

The client's declared input rides on Link rather than in a new Mutex on
Shared. run_actions already clones the link at its top, so reading it
costs no lock that is not taken today, and a disconnect clears it by
replacing the whole Link.

The hotkey asks both machines for the same target, because the machine
that wants the screen back is by definition not the one the monitor is
listening to. One of the two commands lands and the other fails
harmlessly.

A warning fires when both machines claim the same input. That is the
likeliest misconfiguration -- both ends are set up from the same
instructions and the number is easy to copy across -- and its symptom is
a feature that silently does nothing at all.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 9: Client wiring

**Files:**
- Modify: `crates/pheme-app/src/client.rs`

**Interfaces:**
- Consumes: `DisplayService` (Task 7), `Msg::{Hello, HelloAck, SwitchDisplay}` (Task 5), `DisplayCfg` (Task 4).
- Produces: nothing later tasks consume.

- [ ] **Step 1: Send this machine's input in `Hello`**

At `crates/pheme-app/src/client.rs:367`, replace `display_input: None,` with `display_input: cfg.display.input,`.

- [ ] **Step 2: Keep the server's input from `HelloAck`**

Change the `HelloAck` destructure from `display_input: _,` to `display_input: server_display_input,` and carry that `Option<u16>` into the message loop as a local, beside the `clipboard` handle that already travels the same path.

- [ ] **Step 3: Spawn the service**

At `crates/pheme-app/src/client.rs:587`, beside the clipboard:

```rust
    let display = {
        let monitor = cfg.display.monitor.clone();
        DisplayService::spawn(
            &cfg.display,
            Box::new(move || pheme_display::open(monitor.as_deref())),
        )
    };
```

and thread it to the message loop the way `clipboard` is threaded.

- [ ] **Step 4: Hook the crossing back**

At `crates/pheme-app/src/client.rs:444`, extend the existing `Msg::Leave` block:

```rust
                    if matches!(m, Msg::Leave { .. }) {
                        if let Some(c) = &clipboard {
                            c.send_to(sender.clone());
                        }
                        // The pointer is going back to the server and the
                        // picture goes with it. The client is the displayed
                        // input at this moment, so this is the only command
                        // that can reach the monitor.
                        if let (Some(d), Some(v)) = (&display, server_display_input) {
                            d.switch_to(v);
                        }
                    }
```

- [ ] **Step 5: Handle an incoming `SwitchDisplay`**

In the same match, beside the `Leave` arm:

```rust
                    // `&m`, not `m`: `Msg` is not `Copy` and the loop
                    // still passes it to `core.on_msg` below.
                    if let Msg::SwitchDisplay { input } = &m {
                        // Review Focus 4: a no-op when this machine has no
                        // monitor configured, which is the whole handling.
                        if let Some(d) = &display {
                            d.force(input);
                        }
                    }
```

- [ ] **Step 6: Verify by hand that the two hooks agree**

Run: `rg -n 'switch_to|force\(' crates/pheme-app/src/{client,server}.rs`
Expected: exactly one `switch_to` per file (the crossing) and `force` only on the hotkey and the received `SwitchDisplay`. A second `switch_to` anywhere means a crossing is being commanded twice.

- [ ] **Step 7: Run the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 8: Commit**

```bash
git add crates/pheme-app/src/client.rs
git commit -F - <<'EOF'
Update: switch the monitor when the client hands the pointer back

The mirror of the server's half, at the Msg::Leave branch that already
carries the clipboard home. The client is the displayed input at that
moment, so it is the only machine whose DDC/CI command the monitor will
answer -- which is why this half cannot live on the server.

The client sends its own display.input in Hello and keeps the server's
from HelloAck, so each end knows the number it needs to command when it
hands the pointer over.

An incoming SwitchDisplay is forced through when this machine has a
monitor configured and ignored when it does not, so a peer with monitor
control can safely ask a peer without it.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 10: `pheme displays` and `pheme setup`

**Files:**
- Modify: `crates/pheme-app/src/main.rs`
- Modify: `crates/pheme-app/src/setup.rs`

**Interfaces:**
- Consumes: `pheme_display::{enumerate, caps::input_values_from_caps}` (Tasks 2, 3).
- Produces: the `pheme displays` subcommand.

Without this command the feature cannot be configured: VCP input values are vendor-specific and nothing else on the machine prints them.

- [ ] **Step 1: Write the failing test for the setup constants**

In the `tests` module of `crates/pheme-app/src/setup.rs`, beside the existing two:

```rust
    /// Break it by replacing rather than appending: uinput stops being
    /// loaded and virtual input devices stop working, which is the whole
    /// of sub-project 1.
    #[test]
    fn modules_load_entry_names_both_modules() {
        let lines: Vec<&str> = MODULES_LOAD.lines().collect();
        assert!(lines.contains(&"uinput"), "{MODULES_LOAD:?}");
        assert!(lines.contains(&"i2c-dev"), "{MODULES_LOAD:?}");
    }

    /// Break it by matching `KERNEL=="i2c*"`: that also matches the
    /// `i2c-dev` bus devices' parents and other i2c character devices this
    /// rule has no business relaxing.
    #[test]
    fn udev_rule_covers_the_i2c_buses() {
        assert!(
            UDEV_RULE.contains(r#"KERNEL=="i2c-[0-9]*""#),
            "{UDEV_RULE}"
        );
        assert!(UDEV_RULE.contains(r#"GROUP="i2c""#), "{UDEV_RULE}");
        // uaccess is what makes this work without group membership, the
        // same way the uinput rule already does.
        assert_eq!(UDEV_RULE.matches(r#"TAG+="uaccess""#).count(), 2);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib setup`
Expected: FAIL — the assertions on `i2c-dev` and the i2c rule.

- [ ] **Step 3: Extend the constants**

In `crates/pheme-app/src/setup.rs`:

```rust
pub const UDEV_RULE: &str = "# Pheme: allow members of the input group to create virtual input devices\nKERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\", TAG+=\"uaccess\"\n# Pheme: allow the logged-in user to speak DDC/CI to monitors over i2c\nKERNEL==\"i2c-[0-9]*\", MODE=\"0660\", GROUP=\"i2c\", TAG+=\"uaccess\"\n";

/// Makes systemd load `uinput` (virtual input devices) and `i2c-dev`
/// (DDC/CI monitor control) at boot; `modprobe` alone does not survive a
/// reboot on most distributions.
pub const MODULES_LOAD: &str = "uinput\ni2c-dev\n";
```

Update the instructions printed when setup runs without root (`setup.rs:30-33`) so the `modprobe` line names both modules:

```rust
        println!(
            "  modprobe uinput && modprobe i2c-dev && printf 'uinput\\ni2c-dev\\n' > {}",
            modules_path.display()
        );
```

and, at the end of a successful run, print the group fallback rather than acting on it:

```rust
    println!(
        "If DDC/CI still fails, add yourself to the i2c group and log in again:\n  \
         usermod -aG i2c {user}"
    );
```

`pheme setup` does not change group membership on its own. That is a lasting change to a person's account for a feature they may not use.

- [ ] **Step 4: Add the subcommand**

In `crates/pheme-app/src/main.rs`, in `enum Cmd`, after `Devices`:

```rust
    /// List the monitors this machine can switch, and the inputs they take
    Displays,
```

and in `run_subcommand`:

```rust
        Cmd::Displays => {
            let mut found = pheme_display::enumerate();
            if found.is_empty() {
                println!(
                    "No monitor answered DDC/CI.\n\
                     On Linux, run `pheme setup` and check that /dev/i2c-* is readable; \
                     `ddcutil detect` is a useful second opinion.\n\
                     Many monitors also have a DDC/CI switch in their on-screen menu, \
                     and some laptop docks and adapters do not carry the i2c lines at all."
                );
                return Ok(());
            }
            println!("{:<40} {:<14} {:<8} SUPPORTED", "IDENTITY", "LOCATION", "CURRENT");
            for m in found.iter_mut() {
                // Read before the borrow of `m` is split across the two
                // calls below; both take `&mut self`.
                let current = match m.get_input() {
                    Ok(v) => format!("0x{v:02x}"),
                    Err(_) => "-".to_string(),
                };
                // Advisory: plenty of monitors return no capability string,
                // or one that omits inputs they do accept. The current
                // value above is the reliable half -- switch the input by
                // hand, re-run this, and read off the number.
                let supported = match m.capabilities() {
                    Ok(caps) => {
                        let vals = pheme_display::caps::input_values_from_caps(&caps);
                        if vals.is_empty() {
                            "-".to_string()
                        } else {
                            vals.iter()
                                .map(|v| format!("0x{v:02x}"))
                                .collect::<Vec<_>>()
                                .join(" ")
                        }
                    }
                    Err(_) => "-".to_string(),
                };
                println!(
                    "{:<40} {:<14} {:<8} {}",
                    m.identity(),
                    m.location(),
                    current,
                    supported
                );
            }
            Ok(())
        }
```

Add `pheme-display = { workspace = true }` to `crates/pheme-app/Cargo.toml` if Task 7 did not already (it did).

- [ ] **Step 5: Run it**

Run: `cargo run -p pheme-app -- displays`
Expected: on a machine whose monitors ignore DDC/CI, the "No monitor answered DDC/CI" text and exit 0 — not an error. That is the case on the development machine, and it is the correct output there.

- [ ] **Step 6: Run the tests and the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 7: Commit**

```bash
git add crates/pheme-app/src/main.rs crates/pheme-app/src/setup.rs
git commit -F - <<'EOF'
Update: add pheme displays and the i2c setup steps

Without this command the feature cannot be configured. VCP input values
are vendor-specific -- MCCS assigns 0x0F to DisplayPort-1 and vendors
disregard it freely -- and nothing else on the machine prints them.

The capability string's list is advisory and printed as such: plenty of
monitors return none, or omit inputs they do accept. The currently
selected input beside it is the reliable half, because switching by hand
and re-running the command reads the number straight off the hardware.

Finding nothing is not an error. It is the common case, it exits zero,
and the message names the three things that actually cause it: an i2c
device nobody can read, a DDC/CI switch turned off in the monitor's menu,
and a dock or adapter that does not carry the i2c lines.

setup appends i2c-dev beside uinput and adds a udev rule for the i2c
buses, tagged uaccess so the logged-in user needs no group membership. It
prints the usermod fallback rather than running it: group membership is a
lasting change to an account, for a feature the person may not use.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 11: The front-end button and status line

**Files:**
- Modify: `crates/pheme-app/src/ipc/proto.rs`
- Modify: `crates/pheme-app/src/frontend/tray.rs`
- Modify: `crates/pheme-app/src/frontend/window.rs`
- Modify: `crates/pheme-app/src/server.rs`, `crates/pheme-app/src/client.rs` (handle the new `Command`)

**Interfaces:**
- Consumes: `Command` (existing), `DisplayService::force` (Task 7).
- Produces: `Command::SwitchDisplay`, `TrayEvent::SwitchDisplay`.

- [ ] **Step 1: Write the failing test**

In the `tests` module of `crates/pheme-app/src/frontend/tray.rs`, extend the existing id-mapping test rather than adding a second one:

```rust
        assert_eq!(
            event_for_id(ID_SWITCH_DISPLAY),
            Some(TrayEvent::SwitchDisplay)
        );
```

and in `crates/pheme-app/src/ipc/proto.rs`'s tests, beside the existing frame round-trips:

```rust
    /// Break it by inserting SwitchDisplay before Stop in the enum: the
    /// front-end and a core built from a different commit then disagree
    /// about which number means "stop", and Save-with-restart kills the
    /// wrong thing.
    #[test]
    fn command_variants_keep_their_order() {
        for (c, want) in [
            (Command::Lock, 0u8),
            (Command::Unlock, 1),
            (Command::Stop, 2),
            (Command::SwitchDisplay, 3),
        ] {
            assert_eq!(postcard::to_stdvec(&c).unwrap(), vec![want]);
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pheme-app --lib ipc frontend`
Expected: FAIL — `no variant named SwitchDisplay`.

- [ ] **Step 3: Add the command**

In `crates/pheme-app/src/ipc/proto.rs`, append to `Command` — after `Stop`, so no existing variant's postcard index moves:

```rust
    /// Re-assert the monitor input for the machine that holds the pointer.
    /// The window's and the tray's route to the same thing the
    /// `hotkeys.switch_display` key does.
    SwitchDisplay,
```

- [ ] **Step 4: Add the tray item**

In `crates/pheme-app/src/frontend/tray.rs`: a `TrayEvent::SwitchDisplay` variant, an `ID_SWITCH_DISPLAY` constant beside the others, a `MenuItem::with_id(ID_SWITCH_DISPLAY, "Switch display", false, None)` built and appended to the menu beside `start_stop`, a field on `Tray` holding it, its arm in `event_for_id`, and `set_enabled(running)` on it wherever `set_state` already does that for `lock`.

- [ ] **Step 5: Add the window button and the method both callers share**

In `crates/pheme-app/src/frontend/window.rs`, beside `toggle_lock`:

```rust
    /// Re-asserts the monitor's input for whichever machine holds the
    /// pointer. The only place the tray's "Switch display" item and the
    /// window's own button drive, for the same reason `toggle_lock` is:
    /// two callers reimplementing one action drift apart.
    ///
    /// The hotkey remains the one that matters. When the monitor is showing
    /// the wrong machine the person cannot see this window at all, which is
    /// exactly the case the feature exists for.
    fn switch_display(&mut self) {
        if matches!(self.supervisor.state(), CoreState::Running(_)) {
            self.handle.block_on(self.supervisor.send(Command::SwitchDisplay));
        }
    }
```

its `TrayEvent::SwitchDisplay => self.switch_display(),` arm in `PhemeApp::update` beside `TrayEvent::ToggleLock`, and in `draw_actions`, after the Lock button inside the same `ui.horizontal`:

```rust
        if ui
            .add_enabled(running, egui::Button::new("Switch display"))
            .clicked()
        {
            app.switch_display();
        }
```

- [ ] **Step 6: Handle the command in both roles**

Wherever `Command::Lock` / `Command::Unlock` / `Command::Stop` are handled in `crates/pheme-app/src/server.rs` and `crates/pheme-app/src/client.rs`, add an arm that does what the hotkey does. On the server, that is `Action::SwitchDisplay { local: <whether the core is local> }` fed through `execute`, so the one implementation in Task 8 serves both:

```rust
            Command::SwitchDisplay => {
                // Two statements, deliberately. The temporary guard drops at
                // the end of the `let`, so `execute` -- which locks `core`
                // itself on several paths -- never runs while this holds it.
                let local = matches!(shared.core.lock().unwrap().active(), Active::Local);
                shared.execute(vec![Action::SwitchDisplay { local }]);
            }
```

On the client there is no core to ask and no peer to tell — the server owns the hotkey — so the client re-asserts its own known server input:

```rust
            Command::SwitchDisplay => {
                if let (Some(d), Some(v)) = (&display, server_display_input) {
                    d.force(v);
                }
            }
```

- [ ] **Step 7: Add the status line**

In the status panel, beside the "Locked" label, show what the display service is doing. `Status` gains one field:

```rust
    /// The monitor input this machine last successfully commanded, or
    /// `None` when display switching is off or nothing has been commanded
    /// yet. Rendered as a hex value, because that is how `display.input` is
    /// written and how `pheme displays` prints it.
    pub display_input: Option<u16>,
```

filled from `DisplayService::last_input()` (Task 7) wherever the rest of
`Status` is assembled — `shared.display.as_ref().and_then(|d| d.last_input())`
on the server, the same expression over the client's own handle — and drawn
as:

```rust
    ui.label(match status.display_input {
        Some(v) => format!("Display input 0x{v:02x}"),
        None => "Display switching off".to_string(),
    });
```

- [ ] **Step 8: Run the tests and the full gate**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

- [ ] **Step 9: Commit**

```bash
git add crates/pheme-app/src
git commit -F - <<'EOF'
Update: put Switch display in the window and the tray

Both callers go through one method, the way Start/Stop and Lock already
do after sub-project 6's final review found them duplicated. The button
lives in the window as well as the tray because GNOME without the
AppIndicator extension -- the README's own stated default there -- has no
tray at all.

The button is the convenience and the hotkey is the one that matters:
when the monitor is showing the wrong machine, the person cannot see this
window, which is the entire case the feature exists for.

Command::SwitchDisplay is appended after Stop so no existing variant's
postcard index moves, and a test pins those indices: a front-end and a
core built from different commits must not disagree about which number
means stop.

The status panel reports the input last successfully commanded, so a
person can tell "switching is off" from "switched, and the monitor
ignored it".

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```

---

### Task 12: Documentation and the final gate

**Files:**
- Modify: `README.md`
- Modify: `docs/testing.md`

**Interfaces:**
- Consumes: everything.
- Produces: nothing.

- [ ] **Step 1: Document the configuration**

In `README.md`, beside the existing `[audio]` and `[hotkeys]` documentation:

```markdown
### Sharing one monitor between both machines

If a single monitor is cabled to both machines, pheme can make the picture
follow the pointer: crossing the screen edge switches the monitor's input
source to the machine that now has the keyboard and mouse.

Run `pheme displays` on each machine to find the value for the input that
machine is cabled to:

    IDENTITY                             LOCATION      CURRENT  SUPPORTED
    GSM LG ULTRAGEAR (106NTMXE1579)      /dev/i2c-10   0x11     0x0f 0x11 0x12

Then set it on **each machine**, to that machine's own input:

```toml
[display]
# The VCP 0x60 value of the input THIS machine is cabled to.
input = 0x11
# Optional: a substring of the identity above, when more than one monitor
# answers.
monitor = "ULTRAGEAR"
# Optional: minimum gap between switches, in milliseconds.
cooldown_ms = 1000

[hotkeys]
# Optional: re-assert the input for whichever machine has the pointer, for
# when the monitor missed a command. Worth setting -- when the screen shows
# the wrong machine you cannot reach the window or the tray.
switch_display = "F12"
```

The two machines must **not** use the same value: each names its own cable.

Requirements and limits:

- The monitor must answer DDC/CI. Many do not, and some have a DDC/CI
  switch in their on-screen menu that ships turned off. Check with
  `pheme displays`, or with `ddcutil detect` on Linux.
- Some laptop docks and HDMI adapters do not carry the i2c lines the
  protocol needs.
- On Linux, `pheme setup` loads `i2c-dev` and adds the udev rule. If it
  still fails, `usermod -aG i2c $USER` and log in again.
- With no `[display]` section, nothing here runs and nothing changes.
```

- [ ] **Step 2: Add the manual test rows**

In `docs/testing.md`, a new section after "Tray and configuration GUI (sub-project 6)":

```markdown
## Display input switching (sub-project 7)

These need one monitor cabled to both machines, and a monitor that answers
DDC/CI. They are the only tests that exercise real hardware: everything
automated runs against a mock.

| # | Action | Pass |
|---|---|---|
| E1 | `pheme displays` on each machine | each monitor is listed with a plausible identity and a current input |
| E2 | Set `display.input` on both, cross the edge | the monitor shows the client within ~2 s |
| E3 | Cross back | the monitor shows the server |
| E4 | Sweep across the edge and back inside one second | the monitor ends on the machine the pointer ended on, and switches at most once |
| E5 | Press the `switch_display` hotkey while the monitor is on the wrong machine | the monitor corrects itself |
| E6 | Click "Switch display" in the window | the same |
| E7 | Run with `display.input` set where the monitor ignores DDC/CI | one warning at startup, nothing later, input and audio unaffected |
| E8 | Remove `[display]`, cross the edge | no DDC traffic, no warning, everything else unchanged |
| E9 | Unplug the monitor's second cable, cross the edge | the failed command warns once and does not repeat on later crossings |
| E10 | Give both machines the same `display.input` and connect | the "same monitor input" warning appears on the server |
```

- [ ] **Step 3: Run the full gate one last time**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
RUSTUP_HOME=$HOME/.rustup-standalone CARGO_HOME=$HOME/.cargo-standalone \
  PATH=$HOME/.cargo-standalone/bin:$PATH \
  cargo check -p pheme-display --target x86_64-pc-windows-gnu --all-targets
```

- [ ] **Step 4: Confirm the lock file is committed**

```bash
git status --porcelain Cargo.lock
```
Expected: empty. `release.yml` builds `--locked`; an uncommitted lock file breaks the release job while the test job silently regenerates it.

- [ ] **Step 5: Commit**

```bash
git add README.md docs/testing.md
git commit -F - <<'EOF'
Update: document display switching and add its manual test rows

The README section leads with `pheme displays`, because the numbers are
vendor-specific and nothing else on the machine prints them, and states
the one rule a person can get wrong from the instructions alone: each
machine names its own cable, so the two values must differ.

E1 to E10 are the only tests that touch real hardware. Everything
automated runs against a mock, and the development machine's monitor does
not answer DDC/CI at all, so these rows carry the whole of the evidence
that the feature works.

Co-Authored-By: Claude <noreply@anthropic.com>
EOF
```
