# rflow

`rflow` is a low-latency keyboard and mouse sharing MVP. Linux and macOS can act as the controlling
computer; Linux, Windows, and macOS can act as a host. Windows input capture is not implemented.
Pointer motion uses QUIC datagrams and a latest-value slot, so stale movement is discarded instead
of building a queue. Keys, mouse buttons, and wheel events use a reliable ordered QUIC stream.

## MVP capabilities

- Capture one or more Linux evdev devices.
- Inject through Linux `/dev/uinput`, Windows `SendInput`, or macOS CoreGraphics.
- TLS 1.3 authentication and encryption with an explicitly copied self-signed certificate.
- Coalesce `REL_X` and `REL_Y` within an evdev batch and discard stale network datagrams.
- Release held keys/buttons when the sender disconnects or requests shutdown.
- IPv4 and IPv6 support, heartbeat, protocol version check, and structured logs.

Not included yet: automatic discovery, GUI configuration, clipboard, screen-edge switching,
multi-receiver sessions, or support for Windows and macOS.

## Build

```bash
cargo build --release
```

A Linux controlling computer must be allowed to read the selected `/dev/input/event*` devices. A
Linux host must be allowed to write `/dev/uinput`. Windows uses the Win32 `SendInput` API. macOS
capture and injection require Accessibility and Input Monitoring permission for the terminal or
`rflow`. Distribution packages commonly provide an `input` group, but group names and recommended
udev policy vary.

## Quick start

On the computer you want to control, generate its identity once:

```bash
rflow keygen
```

Copy `rflow-cert.der` to the controlling computer over a trusted channel. Never copy
`rflow-key.der` away from the host. Start the host:

```bash
RUST_LOG=rflow=info rflow host \
  --bind 0.0.0.0:24801 \
  --cert rflow-cert.der \
  --key rflow-key.der
```

Find the keyboard and mouse event nodes on the controlling computer:

```bash
ls -l /dev/input/by-id/
```

Prefer stable `/dev/input/by-id/...-event-kbd` and `...-event-mouse` paths. Then connect:

```bash
RUST_LOG=rflow=info rflow connect 192.168.1.50:24801 \
  --cert rflow-cert.der \
  --device /dev/input/by-id/your-keyboard-event-kbd \
  --device /dev/input/by-id/your-mouse-event-mouse
```

On macOS, no device paths are needed:

```bash
RUST_LOG=rflow=info rflow connect 192.168.1.50:24801 \
  --cert rflow-cert.der
```

Without `--grab`, events affect both machines, which is the safest first test. Once SSH or another
emergency recovery path is available, add `--grab` to make rflow exclusively consume those
devices. Press Ctrl-C to stop when running without `--grab`. With grabbed keyboards, Ctrl-C is
sent to the host, so stop `rflow connect` through SSH or another independent input device. Do not
test `--grab` without that recovery path.

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

The copied certificate pins the receiver identity. QUIC encrypts input in transit. The MVP does
not authenticate the sender to the receiver, so expose UDP port 24801 only to a trusted LAN or
restrict it with a host firewall. Mutual authentication/pairing is planned beyond the MVP.
