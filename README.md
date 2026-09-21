# Pheme

Share one machine's keyboard and mouse with another over the LAN, Deskflow-style,
with bidirectional audio forwarding (planned). Written in Rust. GPL-3.0.

Status: early development — see `docs/superpowers/specs/` for the design.

## Build

    cargo build --release

## Run (sub-project 1: keyboard/mouse only)

    # on the server (the machine with the keyboard and mouse)
    pheme server --pair            # prints a 6-digit pairing code, once
    pheme server

    # on the client
    pheme pair <server-ip> <code>  # once
    pheme client <server-ip>

See `docs/testing.md` for the manual test checklist.
