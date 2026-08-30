# rflow

`rflow` is a low-latency virtual KVM MVP. The host owns the physical keyboard/mouse and screen
layout; a client is a remote screen that receives input only after the cursor crosses onto it.
Pointer motion uses QUIC datagrams and a latest-value slot, so stale movement is discarded instead
of building a queue. Keys, mouse buttons, and wheel events use a reliable ordered QUIC stream.

## MVP capabilities

- Capture one or more Linux evdev devices.
- Inject through Linux `/dev/uinput`, Windows `SendInput`, or macOS CoreGraphics.
- Place one client in any of eight directions around the host and switch ownership at the matching
  edge or corner.
- Map the entry coordinate between screens with different resolutions for cardinal layouts.
- TLS 1.3 mutual device authentication with explicit first-connection confirmation.
- Coalesce `REL_X` and `REL_Y` within an evdev batch and discard stale network datagrams.
- Release held keys/buttons when the sender disconnects or requests shutdown.
- IPv4 and IPv6 support, heartbeat, protocol version check, and structured logs.

Not included yet: automatic discovery, GUI configuration, clipboard, arbitrary multi-screen
graphs, or multiple simultaneous clients. The current layout is one host and one client.

## Build

```bash
cargo build --release
```

A Linux host must be allowed to read its selected `/dev/input/event*` devices and write
`/dev/uinput`. Windows uses the Win32 `SendInput` API. macOS capture and injection require
Accessibility and Input Monitoring permission for the terminal or `rflow`. Distribution packages
commonly provide an `input` group, but group names and recommended udev policy vary.

On macOS, enable the actual process launching `rflow` under **System Settings → Privacy &
Security → Accessibility**, then restart that process. If you run the binary from Terminal or
iTerm, authorize that terminal application. rflow logs a warning when macOS does not report the
process as trusted; macOS may still distinguish the responsible terminal or launcher process.

## Quick start

The host and client create persistent device identities automatically. Their default public
certificates are stored at:

```text
Linux:   ~/.config/rflow/identity-cert.der
macOS:   ~/Library/Application Support/rflow/identity-cert.der
Windows: %APPDATA%\rflow\identity-cert.der
```

Never copy `identity-key.der` away from its device. `rflow keygen` and the identity override options
remain available for advanced pre-provisioning. Find stable keyboard and mouse paths:

```bash
ls -l /dev/input/by-id/
```

Start the host with its logical cursor-coordinate size and the client's direction relative to it.
Valid directions are `top`, `top-right`, `right`, `bottom-right`, `bottom`, `bottom-left`, `left`,
and `top-left`. With display scaling enabled, use the logical size (for example, 2560x1440 at 1.6x
is 1600x900):

```bash
RUST_LOG=rflow=info rflow host \
  --bind 0.0.0.0:24801 \
  --size 1600x900 \
  --direction right \
  --device /dev/input/by-id/your-keyboard-event-kbd \
  --device /dev/input/by-id/your-mouse-event-mouse
```

Cardinal layouts cross a shared edge. Diagonal layouts touch at one corner, so the pointer must
move outward across both axes together to cross; it returns through the opposite corner.

On the client screen, connect to the host:

```bash
RUST_LOG=rflow=info rflow client 192.168.1.50 \
  --retry-for 120
```

The client target accepts an IP address or hostname and defaults to UDP port 24801. `--retry-for`
keeps retrying initial connections and reconnecting dropped sessions for the given
number of seconds from client startup. This is useful when one Bluetooth keyboard and mouse must be
switched back to the Linux host before its input devices become available.

On the first connection, both terminals display the same six-digit pairing code. Compare the codes,
then enter `y` on the host to trust the client. rflow does not grab the host input devices until this
confirmation succeeds. Both devices remember the trust relationship for later connections.

For unattended pre-provisioning, the client can additionally pin the expected server certificate:

```bash
rflow client linux-desktop.local --server-cert host-identity-cert.der
```

The host grabs the selected physical devices and reinjects them locally while its own screen is
active. Keep SSH or another independent input device available during MVP testing. When the cursor
is on the client, Ctrl-C is routed to the client rather than the host terminal.

Crossing a screen boundary releases held keys and mouse buttons on the old screen and replays them
on the new screen. This prevents modifiers such as Super/Command from remaining stuck while still
allowing a modifier to be held across the boundary.

## Architecture

rflow separates business decisions from execution and presentation:

- `core` is a pure, deterministic input-routing state machine. `DesktopSession::handle` turns
  domain events into ordered effects and owns physical/local/remote pressed-state invariants.
- `runtime` is the single writer for an active session. It owns Tokio/Quinn, reconnects,
  heartbeat, task supervision, shutdown, application commands/events, and the authoritative
  `RuntimeSnapshot` intended for CLI and future GPUI callers.
- `platform` translates canonical domain input to native Linux/macOS/Windows capture and injection.
- `protocol` explicitly translates domain input to protocol-v2 wire DTOs.
- diagnostics use a separate bounded, lossy worker and cannot block the input hot path.

The CLI only parses arguments, submits `AppCommand`, and renders `AppEvent`. GPUI can use the same
`RuntimeHandle` without moving Quinn or input capture onto the UI thread. See
[`docs/specs/001.md`](docs/specs/001.md) for the full architecture contract.

## Latency design

- Mouse movement is never placed in the reliable input queue.
- A Tokio watch channel stores only the newest pending movement.
- QUIC datagrams are sequence-numbered; late or duplicate movement is ignored.
- A bounded reliable queue applies backpressure to key and button events rather than losing them.
- Input capture uses owned OS threads, separate from the async network runtime, with explicit stop
  and device-release semantics.

Run tests and static checks with:

```bash
cargo test
cargo clippy --all-targets --all-features -- -D warnings
```

## Security notes

QUIC encrypts input in transit and both devices prove possession of their long-term private keys.
An unknown device is not trusted until the host accepts a transcript-bound pairing code. Users must
compare the code shown on both devices to detect a first-connection intermediary. The full security
model is specified in [`docs/specs/002.md`](docs/specs/002.md) and
[`docs/adr/001-device-pairing-protocol.md`](docs/adr/001-device-pairing-protocol.md).
