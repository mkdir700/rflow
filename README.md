# rflow

`rflow` is a low-latency virtual KVM MVP. The host owns the physical keyboard/mouse and screen
layout; a client is a remote screen that receives input only after the cursor crosses onto it.
Pointer motion uses QUIC datagrams and a latest-value slot, so stale movement is discarded instead
of building a queue. Keys, mouse buttons, and wheel events use a reliable ordered QUIC stream.

## MVP capabilities

- Capture one or more Linux evdev devices.
- Inject through Linux `/dev/uinput`, Windows `SendInput`, or macOS CoreGraphics.
- Place one client to the right of the host and switch ownership at the shared screen edge.
- Map the entry height between screens with different resolutions.
- TLS 1.3 authentication and encryption with an explicitly copied self-signed certificate.
- Coalesce `REL_X` and `REL_Y` within an evdev batch and discard stale network datagrams.
- Release held keys/buttons when the sender disconnects or requests shutdown.
- IPv4 and IPv6 support, heartbeat, protocol version check, and structured logs.

Not included yet: automatic discovery, GUI configuration, clipboard, arbitrary screen graphs, or
multiple simultaneous clients. The current layout is one client to the host's right.

## Build

```bash
cargo build --release
```

A Linux host must be allowed to read its selected `/dev/input/event*` devices and write
`/dev/uinput`. Windows uses the Win32 `SendInput` API. macOS capture and injection require
Accessibility and Input Monitoring permission for the terminal or `rflow`. Distribution packages
commonly provide an `input` group, but group names and recommended udev policy vary.

## Quick start

On the host—the computer with the physical keyboard and mouse—generate its identity once:

```bash
rflow keygen
```

Copy `rflow-cert.der` to the client over a trusted channel. Never copy `rflow-key.der` away from
the host. Find stable keyboard and mouse paths:

```bash
ls -l /dev/input/by-id/
```

Start the host with its logical cursor-coordinate size and the single client on its right. With
display scaling enabled, use the logical size (for example, 2560x1440 at 1.6x is 1600x900):

```bash
RUST_LOG=rflow=info rflow host \
  --bind 0.0.0.0:24801 \
  --size 1600x900 \
  --right \
  --cert rflow-cert.der \
  --key rflow-key.der \
  --device /dev/input/by-id/your-keyboard-event-kbd \
  --device /dev/input/by-id/your-mouse-event-mouse
```

On the client screen, connect to the host:

```bash
RUST_LOG=rflow=info rflow client 192.168.1.50:24801 \
  --cert rflow-cert.der
```

The host grabs the selected physical devices and reinjects them locally while its own screen is
active. Keep SSH or another independent input device available during MVP testing. When the cursor
is on the client, Ctrl-C is routed to the client rather than the host terminal.

## Latency design

- Mouse movement is never placed in the reliable input queue.
- A Tokio watch channel stores only the newest pending movement.
- QUIC datagrams are sequence-numbered; late or duplicate movement is ignored.
- A bounded reliable queue applies backpressure to key and button events rather than losing them.
- Input capture uses blocking OS threads, separate from the async network runtime.

Run tests and static checks with:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Security notes

The copied certificate pins the host identity. QUIC encrypts input in transit. The MVP does not
authenticate the client to the host, so expose UDP port 24801 only to a trusted LAN or restrict it
with a host firewall. Mutual authentication/pairing is planned beyond the MVP.
