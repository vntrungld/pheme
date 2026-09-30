# Sub-project 9 — Automated Cross-OS Test Rig (Tier 1)

Date: 2026-09-30
Status: awaiting spec review
License: GPL-3.0-only

## 1. Why this exists

Across eight sub-projects, **not one row of the manual cross-OS matrix in
`docs/testing.md` has ever been run.** Every claim about pheme working
between two machines rests on unit tests, integration tests with mock
backends, and container checks. All of those are real, and none of them has
ever moved a pointer from one operating system to another.

That is the single largest gap in the project, and it is not a gap more unit
tests can close: the matrix exists precisely because the behaviour it
describes only appears when two real machines, two real input stacks and two
real audio stacks are involved.

This sub-project builds a rig that runs the machine-checkable part of that
matrix on demand, and reports each row as passed, failed, skipped or
inconclusive with the evidence behind the verdict.

## 2. Goals

1. One command runs the matrix rows a program can judge, and prints a table.
2. Assertions read the **operating system**, not pheme's own reports.
3. A row that was not run says so. A row that ran but could not be judged
   says so. Neither is ever reported as a pass.
4. The rig can prove it is capable of failing (§13).
5. Repeated runs are trustworthy: every run starts from an identical machine
   state.

## 3. Non-goals

- **Replacing the manual matrix.** Some rows need a human: audio judged by
  ear, and anything needing a real monitor (§11).
- **Running in CI on every push.** The rig runs when asked. A self-hosted
  runner is a later decision, and nothing here should make it harder.
- **Being a substitute for real hardware.** Two VMs are not two laptops.
  Timing characteristics differ, and the rig's RTT figures are not a latency
  benchmark — §11 says which rows this weakens. The manual matrix stays.
- **Windows→Windows or Linux→Linux.** Tier 1 covers the two cross-OS
  directions, which are the ones no evidence exists for. A second VM of
  either kind is an increment, not a redesign.

## 4. Architecture

Five parts, under a new top-level `testrig/` directory — the same shape as
`packaging/`, which already orchestrates containers in shell.

| Part | Responsibility |
|---|---|
| `testrig/rig.conf` | Declares the two VMs: name, OS, RAM, disk, network. One place to read what the rig is. |
| `testrig/provision-ubuntu.sh`, `provision-windows.sh` | Build each VM unattended. Idempotent. |
| `testrig/net.sh` | Create and remove the rig's own libvirt network. |
| `testrig/run` | Revert to snapshot, boot, deploy, run rows, collect, report, shut down. |
| `crates/pheme-probe/` | The instrument that emits input on one machine and records it on the other. |

The shell parts own VM lifecycle, because `virsh` is a command-line tool and
shelling out to it from a program buys nothing. The probe is Rust, because
reading evdev and Windows input queues is real programming and the workspace
already has the types for it.

## 5. The VM layer

Prerequisites, all verified present on the development machine on
2026-09-30:

| Need | Found |
|---|---|
| Hardware virtualisation | Intel Core Ultra 9 285H, VT-x, `/dev/kvm` readable |
| Memory headroom | 30 GiB total, ~16 GiB available |
| Disk | 306 GiB free |
| `libvirt` / `qemu` / `virt-install` | installed; `virt-install` 5.1.0, with `--cloud-init` and `--tpm` |
| ISO authoring | `xorriso` (no `genisoimage`; `xorriso` substitutes) |
| **TPM 2.0 emulation** | `swtpm`, `swtpm_setup` — **required**: Windows 11 Setup refuses a machine with no TPM 2.0, and this rig uses the supported path rather than a registry bypass |
| **UEFI firmware** | `/usr/share/edk2/x64/OVMF_CODE.4m.fd` — **required**, Windows 11 needs UEFI + Secure Boot |

Those last two are why this is feasible at all. A rig design that assumed
BIOS and no TPM would have failed at the Windows installer.

**Ubuntu VM.** Built from the Ubuntu 24.04 **cloud image** plus
`virt-install --cloud-init`, then `apt install ubuntu-desktop` on first
boot. Not the desktop ISO with autoinstall: cloud-init is a supported,
documented unattended path, whereas driving the desktop installer is
screen-scraping. The result is the same GNOME-on-Wayland session a person
would have, and an X11 session is selectable from the same VM.

**Windows VM.** Built from the Windows 11 Enterprise Evaluation ISO
(free, time-limited, re-creatable) with an `autounattend.xml` on a seed ISO
built by `xorriso`. The answer file creates a **standard, non-administrator
user** — F7's whole point is that the installer needs no elevation, and an
administrator account would make that row pass for the wrong reason — and
enables **OpenSSH Server**, which ships with Windows as an optional feature.

Both VMs are given a clean snapshot once provisioning finishes (§12).

## 6. Network

One libvirt **NAT** network dedicated to the rig, separate from `default`.

NAT, not an isolated network, because provisioning needs outbound internet
for `apt`. NAT, not a bridge onto the real LAN, because the rig must not
advertise pheme services onto the network the developer is using, and
because the host must never be reachable as a pheme peer.

The two VMs share one layer-2 segment, which is required and not incidental:
mDNS discovery is multicast, so `pheme discover` and every SP5 row are
meaningless without it.

## 7. Control channel

SSH from the host to each VM, with a keypair generated into `testrig/` and
listed in `.gitignore`. Not the developer's own key: the rig's key authorises
access to throwaway machines and should never be the key that authorises
anything else.

Windows uses the same channel through OpenSSH Server, so the runner has one
mechanism rather than two.

## 8. Deploy is the packaging test

The rig does **not** copy loose binaries into the VMs. It installs the real
packages: the `.deb` on Ubuntu, the `.exe` installer on Windows.

Two things follow. What gets tested is what a user receives — the same
reasoning that made the packaging checks install inside a clean container
rather than run on the build host. And rows F1, F2, F3 and F6 fall out of the
deploy step instead of needing separate implementations.

Artifacts come from a CI run by default, fetched with
`gh run download <run-id> --name pheme-x86_64-windows` and the matching Linux
artifact, because those are the files a user receives and because the Windows
binary cannot be built on this Linux host at all. `--from-local <dir>` takes a
directory instead, for fast iteration; the report records which source a run
used, since a pass against a local build proves less than one against the
shipped artifact.

## 9. The probe

One crate, `crates/pheme-probe/`, deployed to both VMs, with two modes.

**`emit`** runs on the machine acting as server. It creates a virtual mouse
and keyboard through the same uinput backend `pheme-input` already
implements, and emits the events a row describes: move the pointer 2000 px
right, type 100 characters, hold Shift and cross the edge. pheme's capture
sees them exactly as it sees a person's.

**`record`** runs on the machine acting as client. On Linux it opens the
evdev device **pheme itself created** and counts what arrives. On Windows it
installs a low-level input hook and records what `SendInput` delivered to the
system queue.

The crate is test equipment, not product: it is excluded from the `.deb` and
the `.rpm`, and being in the workspace means `cargo fmt` and
`cargo clippy -D warnings` gate it like everything else.

## 10. Two assertion principles

**Verdicts read the operating system, not pheme's self-report.**

pheme's `--stats` reports `events`, `lost` and `rtt_us`. Using those as the
criterion asks the accused for a verdict: "the server sent 100 events" is not
evidence that 100 keystrokes reached an application on the other machine.
The probe's `record` mode reads the OS input layer, independently of pheme.
`--stats` output is collected as supporting evidence and never as the
criterion.

**Wait on conditions, never on a clock.**

There is no fixed `sleep` anywhere in the rig. After emitting a crossing, the
runner polls the recorder until the expected events arrive or a deadline
expires. This is not a style preference: a fixed sleep in a sub-project 7
test passed on an idle machine and failed under load, and neither outcome
said anything about whether the code was right. The rig runs on a developer
workstation whose load varies by the minute.

## 11. Row coverage

`docs/testing.md`'s sub-project 1 section is ten numbered prose steps with no
identifiers, which a report table cannot reference. This sub-project assigns
them **S1–S10** and edits `docs/testing.md` accordingly.

| Rows | Tier 1 | Note |
|---|---|---|
| S1 pairing, and refusal of an untrusted peer | yes | both halves are log-and-exit-code checkable |
| S2 edge switch, S3 sliding along the edge | yes | the headline rows; probe emits, probe records |
| S4 typing 100 characters | yes | recorded at evdev level rather than into a text editor, which is stronger: it counts what the OS received |
| S5 modifier across the edge | yes | asserts no key is left held, which is a state the recorder can read |
| S6 scrolling | yes | notch counts are countable |
| S7 lock hotkey | yes | emit the hotkey, assert no crossing follows |
| S8 client disconnect and reconnect | yes | `virsh domif-setlink <dom> <iface> down` pulls the virtual cable and `up` restores it; the 5 s and 10 s limits become deadlines |
| S9 multi-monitor client | **skipped** | needs a second virtual head; feasible but unverified, deferred rather than guessed at |
| S10 stats, RTT under 1 ms | **inconclusive by design** | the rig reports the number but must not treat a VM's virtio-net RTT as the wired-LAN criterion |
| A1–A11 audio out, M1–M10 mic | partly | presence, frame counts, loss and jitter depth are numbers; "no clicks", "no drift after 30 min" by ear is not. Machine-checkable halves only, and the 30-minute run is opt-in |
| C1–C7, D1–D4 clipboard and discovery | yes | text clipboard and mDNS are both fully checkable |
| W1–W12 Wayland capture | **at risk** | see §15 |
| F1, F2, F3, F6 packaging | yes | from the deploy step (§8) |
| F4, F5, F7–F12 | **skipped** | need a login session, a menu, or a browser download — tier 2 and 3 |
| G1–G10 tray and GUI | **skipped** | need a framebuffer and clicks — tier 2 and 3 |
| E1–E10 display switching | **skipped, permanently** | DDC/CI needs a monitor that answers VCP `0x60` over i2c. No virtual display does. These stay manual on real hardware, where the development machine's monitor is confirmed to answer |

## 12. Lifecycle

```
provision  (once)   →  clean snapshot
run        (each)   →  revert  →  boot  →  wait for ssh  →  install packages
                    →  run rows  →  collect evidence  →  report  →  shut down
```

The revert is what makes a second run mean anything. Rows install and remove
software, write `trusted.toml` during pairing and `config.toml` on first
start. Without reverting, a later run can pass on the previous run's leftovers
rather than on the code.

VMs shut down after a run by default: together they hold more than 12 GiB,
on a machine someone is working on. `--keep` leaves them up while debugging.

## 13. Proving the rig can fail

The rig is code, and code written for this project has produced thirteen
assertions that could not fail. Two guards, both cheap:

**A negative-control run.** `testrig/run --negative-control` runs every row
with pheme **never started**, and asserts that **every row fails**. Any row
that passes with the product switched off is reported as `BROKEN`: whatever
it measures, it is not pheme. This is one extra run, not per-row work, and it
catches the exact defect that recurred through sub-projects 7 and 8.

**Every row names the mutation that breaks it,** recorded beside the row.
The same practice that caught those thirteen claims.

## 14. Verdicts, reporting, and rig failure

Four verdicts, all first class:

| Verdict | Meaning |
|---|---|
| `PASS` | the criterion was evaluated and met |
| `FAIL` | the criterion was evaluated and not met |
| `SKIPPED` | not attempted, with a reason |
| `INCONCLUSIVE` | attempted, but the criterion could not be evaluated |

A run prints a table of row, description, verdict and evidence, and writes
the same content as JSON so a later tier or CI can consume it. Rows that did
not run appear with their reason; they never silently vanish from the table.

**Infrastructure failure is not product failure.** If SSH times out, a VM
does not boot, or the network is down, the affected rows are `INCONCLUSIVE`
with an infrastructure reason, and the report says the run hit a rig error.
They are never `FAIL`. A table blaming pheme for an unstarted libvirt network
is worse than no table.

## 15. Risks

- **The Wayland portal permission dialog.** `InputCapture` asks the user once,
  and a dialog is a click the rig cannot make. There may be a way to pre-grant
  it through the portal's permission store; **this is unverified.** If it
  cannot be done, the W rows run in an X11 session and Wayland capture moves
  to tier 3. This is the largest open question in the design and is called out
  rather than assumed away.
- **A VM is not a laptop.** Timing-sensitive criteria (S10's sub-millisecond
  RTT) cannot be judged here, which is why §11 marks S10 inconclusive by
  design rather than letting it report a comfortable number.
- **The Evaluation image expires** after 90 days. Recovery is re-provisioning,
  which is scripted; the rig must therefore never hold state that only exists
  inside a VM.
- **Provisioning cost.** 30–60 minutes once, dominated by a ~6 GiB Windows ISO
  download and `apt install ubuntu-desktop`. Each later run is 5–15 minutes.
  Worth stating plainly: this is not a fast feedback loop, and it is not
  meant to replace `cargo test`.
- **Audio inside a VM** uses virtual devices. Frame counts and loss are
  meaningful; absolute quality is not.

## 16. Out of scope, and what comes next

Tier 2 adds framebuffer capture through `virsh screenshot`, which reaches
F5, F7 and the G rows — "the window opens", "the tray icon is there", "no UAC
prompt appeared". Tier 3 adds GUI automation that can click, which reaches the
rest, including the Start/Stop button behaviour that a person found by hand on
2026-09-30.

Both are separate sub-projects. Tier 1 is useful on its own and neither of
them changes its design.
