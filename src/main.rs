use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use clap::{Parser, Subcommand};
use quinn::{Connection, RecvStream, SendStream};
use rflow::{
    input::{Injector, spawn_capture},
    protocol::{
        MAX_RELIABLE_FRAME, Motion, PROTOCOL_VERSION, ReliableEvent, decode, encode, encode_frame,
    },
    router::{ActiveScreen, CursorRouter, Route, ScreenSize},
    state::{MotionFilter, PressedState},
    transport,
};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Parser)]
#[command(version, about = "Low-latency virtual KVM over QUIC")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate the host certificate and private key.
    Keygen {
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        #[arg(long, default_value = "rflow-key.der")]
        key: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Own the physical keyboard/mouse and route them across screens.
    Host {
        #[arg(long, default_value = "[::]:24801")]
        bind: SocketAddr,
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        #[arg(long, default_value = "rflow-key.der")]
        key: PathBuf,
        /// Size of this screen in logical cursor coordinates.
        #[arg(long)]
        size: ScreenSize,
        /// Physical Linux evdev path. Repeat for keyboard and mouse.
        #[arg(long)]
        device: Vec<PathBuf>,
        /// Place the single client screen to the right of this host.
        #[arg(long)]
        right: bool,
    },
    /// Join a host as a remotely controlled screen.
    Client {
        /// Host address, for example 192.168.1.50:24801.
        target: SocketAddr,
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        /// Override automatic screen-size detection.
        #[arg(long)]
        size: Option<ScreenSize>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    match Cli::parse().command {
        Command::Keygen { cert, key, force } => {
            transport::generate_identity(&cert, &key, force)?;
            println!("wrote {} and {}", cert.display(), key.display());
            Ok(())
        }
        Command::Host {
            bind,
            cert,
            key,
            size,
            device,
            right,
        } => host(bind, cert, key, size, device, right).await,
        Command::Client { target, cert, size } => client(target, cert, size).await,
    }
}

async fn host(
    bind: SocketAddr,
    cert: PathBuf,
    key: PathBuf,
    local_size: ScreenSize,
    devices: Vec<PathBuf>,
    right: bool,
) -> Result<()> {
    if !right {
        bail!("the MVP supports one client on the right; pass --right");
    }
    rflow::input::validate_capture(&devices)?;
    let endpoint = transport::server_endpoint(bind, &cert, &key)?;
    tracing::info!(local = %endpoint.local_addr()?, "host listening");
    let connection = transport::accept_one(&endpoint).await?;
    tracing::info!(remote = %connection.remote_address(), "client connected");
    host_connection(connection, local_size, devices).await
}

async fn host_connection(
    connection: Connection,
    local_size: ScreenSize,
    devices: Vec<PathBuf>,
) -> Result<()> {
    let mut metadata = connection
        .accept_uni()
        .await
        .context("accept client metadata stream")?;
    let remote_size = match read_reliable(&mut metadata).await? {
        ReliableEvent::ClientHello {
            version: PROTOCOL_VERSION,
            width,
            height,
        } => ScreenSize::new(width, height).map_err(anyhow::Error::msg)?,
        ReliableEvent::ClientHello { version, .. } => {
            bail!("unsupported client protocol version {version}")
        }
        _ => bail!("first client message must be ClientHello"),
    };

    let mut remote_stream = connection
        .open_uni()
        .await
        .context("open host input stream")?;
    write_reliable(
        &mut remote_stream,
        &ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let mut router = CursorRouter::right(local_size, remote_size);
    let mut local_injector = Injector::new()?;
    if let Some((x, y)) = rflow::input::cursor_position() {
        router.set_local_position(x, y);
    }

    let (reliable_tx, mut reliable_rx) = mpsc::channel(256);
    let (motion_tx, mut motion_rx) = watch::channel(None);
    let _capture_threads = spawn_capture(devices, true, reliable_tx, motion_tx);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat_sequence = 0_u64;
    let mut physical_pressed = PressedState::default();

    loop {
        tokio::select! {
            event = reliable_rx.recv() => {
                let Some(event) = event else { bail!("all host input capture threads stopped") };
                if let ReliableEvent::Input { event_type, code, value, .. } = &event {
                    physical_pressed.observe(*event_type, *code, *value);
                }
                route_reliable(router.active(), &mut local_injector, &mut remote_stream, event).await?;
            }
            changed = motion_rx.changed() => {
                changed.context("all host pointer capture threads stopped")?;
                if let Some(motion) = *motion_rx.borrow_and_update() {
                    route_motion(
                        &connection,
                        &mut remote_stream,
                        &mut local_injector,
                        &mut router,
                        &physical_pressed,
                        motion,
                    ).await?;
                }
            }
            _ = heartbeat.tick() => {
                heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                write_reliable(&mut remote_stream, &ReliableEvent::Heartbeat { sequence: heartbeat_sequence }).await?;
            }
            closed = connection.closed() => bail!("client connection closed: {closed}"),
            _ = tokio::signal::ctrl_c() => {
                let _ = write_reliable(&mut remote_stream, &ReliableEvent::ReleaseAll).await;
                connection.close(0_u32.into(), b"host stopped");
                return Ok(());
            }
        }
    }
}

async fn route_reliable(
    active: ActiveScreen,
    local: &mut Injector,
    remote: &mut SendStream,
    event: ReliableEvent,
) -> Result<()> {
    match (active, event) {
        (
            ActiveScreen::Local,
            ReliableEvent::Input {
                event_type,
                code,
                value,
                ..
            },
        ) => local.emit_raw(event_type, code, value),
        (ActiveScreen::Remote, event @ ReliableEvent::Input { .. }) => {
            write_reliable(remote, &event).await
        }
        _ => Ok(()),
    }
}

async fn route_motion(
    connection: &Connection,
    remote_stream: &mut SendStream,
    local: &mut Injector,
    router: &mut CursorRouter,
    physical_pressed: &PressedState,
    motion: Motion,
) -> Result<()> {
    match router.route_motion(motion.dx, motion.dy) {
        Route::Local { dx, dy } => local.emit_motion(dx, dy),
        Route::Remote { dx, dy } => {
            let routed = Motion { dx, dy, ..motion };
            connection
                .send_datagram(Bytes::from(encode(&routed)?))
                .context("send remote pointer motion")
        }
        Route::EnterRemote { x, y } => {
            tracing::info!(x, y, "cursor entered client screen");
            for (event_type, code) in physical_pressed.held_inputs() {
                local.emit_raw(event_type, code, 0)?;
            }
            write_reliable(remote_stream, &ReliableEvent::EnterScreen { x, y }).await?;
            replay_held_inputs(remote_stream, physical_pressed).await
        }
        Route::EnterLocal { x, y } => {
            tracing::info!(x, y, "cursor returned to host screen");
            write_reliable(remote_stream, &ReliableEvent::ReleaseAll).await?;
            local.set_cursor_position(x, y)?;
            for (event_type, code) in physical_pressed.held_inputs() {
                local.emit_raw(event_type, code, 1)?;
            }
            Ok(())
        }
    }
}

async fn replay_held_inputs(stream: &mut SendStream, pressed: &PressedState) -> Result<()> {
    for (event_type, code) in pressed.held_inputs() {
        write_reliable(
            stream,
            &ReliableEvent::Input {
                sequence: 0,
                event_type,
                code,
                value: 1,
            },
        )
        .await?;
    }
    Ok(())
}

async fn client(to: SocketAddr, cert: PathBuf, requested_size: Option<ScreenSize>) -> Result<()> {
    let size = match requested_size {
        Some(size) => size,
        None => rflow::input::screen_size()?,
    };
    let bind_ip = if to.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let endpoint = transport::client_endpoint(SocketAddr::new(bind_ip, 0), &cert)?;
    let connection = transport::connect(&endpoint, to).await?;
    tracing::info!(remote = %to, width = size.width, height = size.height, "connected to host");

    let mut metadata = connection
        .open_uni()
        .await
        .context("open client metadata stream")?;
    write_reliable(
        &mut metadata,
        &ReliableEvent::ClientHello {
            version: PROTOCOL_VERSION,
            width: size.width,
            height: size.height,
        },
    )
    .await?;
    metadata.finish()?;

    let mut stream = connection
        .accept_uni()
        .await
        .context("accept host input stream")?;
    match read_reliable(&mut stream).await? {
        ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
        } => {}
        ReliableEvent::Hello { version } => bail!("unsupported host protocol version {version}"),
        _ => bail!("first host message must be Hello"),
    }
    client_connection(connection, stream).await
}

async fn client_connection(connection: Connection, mut stream: RecvStream) -> Result<()> {
    let mut injector = Injector::new()?;
    let mut motion_filter = MotionFilter::default();
    let mut pressed = PressedState::default();
    let result: Result<()> = async {
        loop {
            tokio::select! {
                datagram = connection.read_datagram() => {
                    let bytes = datagram.context("read pointer datagram")?;
                    let motion: Motion = decode(&bytes)?;
                    if let Some(motion) = motion_filter.accept(motion) {
                        injector.emit_motion(motion.dx, motion.dy)?;
                    }
                }
                event = read_reliable(&mut stream) => {
                    match event? {
                        ReliableEvent::EnterScreen { x, y } => injector.set_cursor_position(x, y)?,
                        ReliableEvent::Input { event_type, code, value, .. } => {
                            pressed.observe(event_type, code, value);
                            injector.emit_raw(event_type, code, value)?;
                        }
                        ReliableEvent::ReleaseAll => release_all(&mut injector, &mut pressed)?,
                        ReliableEvent::Heartbeat { .. } => {}
                        ReliableEvent::Hello { .. } | ReliableEvent::ClientHello { .. } => {
                            bail!("unexpected handshake message")
                        }
                    }
                }
            }
        }
    }
    .await;
    release_all(&mut injector, &mut pressed)?;
    result
}

fn release_all(injector: &mut Injector, pressed: &mut PressedState) -> Result<()> {
    for (event_type, code) in pressed.drain_releases() {
        injector.emit_raw(event_type, code, 0)?;
    }
    Ok(())
}

async fn write_reliable(stream: &mut SendStream, event: &ReliableEvent) -> Result<()> {
    stream
        .write_all(&encode_frame(event)?)
        .await
        .context("write reliable input event")
}

async fn read_reliable(stream: &mut RecvStream) -> Result<ReliableEvent> {
    let mut header = [0_u8; 4];
    stream
        .read_exact(&mut header)
        .await
        .context("read reliable frame header")?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_RELIABLE_FRAME {
        bail!("reliable frame length {length} exceeds limit");
    }
    let mut body = vec![0_u8; length];
    stream
        .read_exact(&mut body)
        .await
        .context("read reliable frame body")?;
    decode(&body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parses_explicit_right_layout() {
        let cli = Cli::try_parse_from([
            "rflow",
            "host",
            "--size",
            "1920x1080",
            "--right",
            "--device",
            "/dev/input/event1",
        ])
        .unwrap();
        match cli.command {
            Command::Host {
                size,
                right,
                device,
                ..
            } => {
                assert_eq!(
                    size,
                    ScreenSize {
                        width: 1920,
                        height: 1080
                    }
                );
                assert!(right);
                assert_eq!(device, vec![PathBuf::from("/dev/input/event1")]);
            }
            _ => panic!("expected host command"),
        }
    }

    #[test]
    fn client_uses_positional_host() {
        let cli = Cli::try_parse_from(["rflow", "client", "192.168.1.50:24801"]).unwrap();
        match cli.command {
            Command::Client { target, size, .. } => {
                assert_eq!(target, "192.168.1.50:24801".parse().unwrap());
                assert_eq!(size, None);
            }
            _ => panic!("expected client command"),
        }
    }

    #[test]
    fn retired_connect_command_is_not_exposed() {
        assert!(Cli::try_parse_from(["rflow", "connect", "192.168.1.50:24801"]).is_err());
    }
}
