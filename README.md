# rflow

`rflow` is a low-latency virtual KVM MVP. The host owns the physical keyboard/mouse and screen
layout; a client is a remote screen that receives input only after the cursor crosses onto it.
Pointer motion uses QUIC datagrams and a latest-value slot, so stale movement is discarded instead
of building a queue. Keys, mouse buttons, and wheel events use a reliable ordered QUIC stream.

## MVP capabilities

- Capture one or more Linux evdev devices.
- Inject through Linux `/dev/uinput`, Windows `SendInput`, or macOS CoreGraphics.
- Persist named screen-edge links and route across multiple devices and displays.
- Map the entry coordinate between screens with different resolutions for cardinal layouts.
- TLS 1.3 mutual device authentication with explicit first-connection confirmation.
- Coalesce `REL_X` and `REL_Y` within an evdev batch and discard stale network datagrams.
- Release held keys/buttons when the sender disconnects or requests shutdown.
- IPv4 and IPv6 support, heartbeat, protocol version check, and structured logs.

Not included yet: LAN service discovery, GUI configuration, clipboard, or cloud synchronization.

## ICMP tunnel transport

On Linux/IPv4, QUIC packets can optionally be carried inside ICMP echo messages. Both sides must
select the same carrier:

```bash
sudo setcap cap_net_raw=ep ./target/release/rflow
./target/release/rflow host --bind 0.0.0.0:24801
./target/release/rflow client --transport icmp 192.168.1.50:24801
```

The ICMP mode encapsulates rflow's UDP/QUIC datagrams; QUIC still provides TLS 1.3 encryption,
mutual authentication, reliable streams, datagrams, congestion control, and pairing. It does not
replace those properties with a home-grown protocol. The virtual UDP source and destination ports
are included in every tunnel frame, so several rflow endpoints can share the ICMP carrier.

Raw ICMP sockets require `CAP_NET_RAW` (or root). Prefer granting only that capability to the built
binary, and repeat `setcap` after replacing it. ICMP is commonly filtered or rate-limited and can
have worse latency than UDP; this mode is a fallback, not the default. It currently supports Linux
and IPv4 only, so use an IPv4 target address. The normal QUIC transport remains unchanged and is
selected by default on the client. A host listens on both carriers by default; use
`--transport quic` or `--transport icmp` on the host to isolate one carrier during testing.

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
remain available for advanced pre-provisioning.

Start the host. rflow detects the host's active screens, logical dimensions, keyboard, and pointer
automatically:

```bash
rflow host -v
```

Add `-v` for debug logs or `-vv` for full trace logs when diagnosing a runtime problem. The option
can appear before or after the subcommand; an explicit `RUST_LOG` value remains available as an
advanced override.

By default the host listens on `0.0.0.0:24801`, making it reachable over the local network. Use
`--bind 127.0.0.1:24801` when the service should only be available on the same machine.

On Linux, unreadable evdev devices produce a diagnostic listing the affected paths and permission
remedy. `--device PATH` remains an advanced capture override. Use `rflow layout set-size` for a
persistent logical-size override.

On the client screen, connect to the host (use `-v` while diagnosing a connection):

```bash
rflow client 192.168.1.50 -v \
  --retry-for 120
```

The client target accepts an IP address or hostname and defaults to UDP port 24801. `--retry-for`
keeps retrying initial connections and reconnecting dropped sessions for the given
number of seconds from client startup. This is useful when one Bluetooth keyboard and mouse must be
switched back to the Linux host before its input devices become available.

On the first connection, both terminals display the same six-digit pairing code. Compare the codes,
then enter `y` on the host to trust the client. rflow does not grab the host input devices until this
confirmation succeeds. After the client reports its screens, an interactive Host also asks where to
place one unplaced screen relative to the Host's primary screen. Occupied cardinal positions are
shown as unavailable with the occupying screen; choose `0` to leave the device unplaced. Multi-screen
clients first ask which screen to place. Both devices remember the trust relationship for later
connections, which do not repeat either prompt.

Once the peer is connected, inspect the authenticated screen inventory and place its screen. A
single-screen device name can be used as shorthand; multi-screen devices require an exact screen
ID:

```bash
rflow layout screens
rflow layout place macmini --right-of linux-desktop
rflow layout
```

The four relative placement options are `--left-of`, `--right-of`, `--above`, and `--below`.
Configuration is persisted in the platform application directory and applied to the running
session immediately. Layout inspection and editing also work while the Host is stopped; those
changes take effect on its next start. Later `rflow host` invocations reuse the layout without a
direction argument.

Advanced and scripted layout management uses exact edges:

```bash
rflow layout link linux/DP-1.right macmini/main.left
rflow layout unlink linux/DP-1.right
rflow layout unplace macmini/main
rflow layout set-size macmini/main 2560x1440
rflow layout export > layout.json
rflow layout apply layout.json --expected-revision 12
```

`--direction` remains only as a one-time migration aid for an unconfigured, single-remote-screen
cardinal layout. It is rejected once a persistent link exists.

List the remembered devices or explicitly remove one with:

```bash
rflow peers
rflow peers forget macmini
rflow peers forget macmini --yes  # non-interactive scripts
rflow peers forget --all --yes    # remove every trusted device
```

Forgetting a device removes its trust and endpoint bindings. The next connection must be paired
again. Device names may repeat; use the full device ID printed by `rflow peers` when a name is
ambiguous.

When the host runs under systemd, launchd, or another non-interactive process, inspect and decide
the same Runtime's pending request from another terminal:

```bash
rflow peers pending
rflow peers accept p-7f31000000000000
rflow peers reject p-7f31000000000000
```

These commands use an owner-only local management endpoint; they do not start a second host or
write the active Runtime's state directly. On an interactive terminal, the usual `[y/N]` prompt
submits the same Application command.

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
- `protocol` explicitly translates domain input to protocol-v3 wire DTOs, including authenticated
  multi-screen inventories.
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
