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
    state::{MotionFilter, PressedState},
    transport,
};
use tokio::sync::{mpsc, watch};

#[derive(Debug, Parser)]
#[command(version, about = "Low-latency Linux keyboard and mouse sharing")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Generate this host's certificate and private key.
    Keygen {
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        #[arg(long, default_value = "rflow-key.der")]
        key: PathBuf,
        #[arg(long)]
        force: bool,
    },
    /// Make this computer available for remote control.
    Host {
        #[arg(long, default_value = "[::]:24801")]
        bind: SocketAddr,
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        #[arg(long, default_value = "rflow-key.der")]
        key: PathBuf,
    },
    /// Control another computer with this computer's input devices.
    Connect {
        /// Host address, for example 192.168.1.50:24801.
        target: SocketAddr,
        #[arg(long, default_value = "rflow-cert.der")]
        cert: PathBuf,
        #[arg(long, required = true)]
        device: Vec<PathBuf>,
        /// Exclusively grab devices. Use only after verifying the emergency stop path.
        #[arg(long)]
        grab: bool,
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
        Command::Host { bind, cert, key } => receive(bind, cert, key).await,
        Command::Connect {
            target,
            cert,
            device,
            grab,
        } => send(target, cert, device, grab).await,
    }
}

async fn send(to: SocketAddr, cert: PathBuf, devices: Vec<PathBuf>, grab: bool) -> Result<()> {
    let bind_ip = if to.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let endpoint = transport::client_endpoint(SocketAddr::new(bind_ip, 0), &cert)?;
    let connection = transport::connect(&endpoint, to).await?;
    tracing::info!(remote = %to, "connected to receiver");
    let mut stream = connection
        .open_uni()
        .await
        .context("open reliable input stream")?;
    write_reliable(
        &mut stream,
        &ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
        },
    )
    .await?;

    let (reliable_tx, mut reliable_rx) = mpsc::channel(256);
    let (motion_tx, mut motion_rx) = watch::channel(None);
    let _capture_threads = spawn_capture(devices, grab, reliable_tx, motion_tx);
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat_sequence = 0_u64;

    loop {
        tokio::select! {
            event = reliable_rx.recv() => {
                let Some(event) = event else { bail!("all input capture threads stopped") };
                write_reliable(&mut stream, &event).await?;
            }
            changed = motion_rx.changed() => {
                changed.context("all pointer capture threads stopped")?;
                let motion = *motion_rx.borrow_and_update();
                if let Some(motion) = motion {
                    connection.send_datagram(Bytes::from(encode(&motion)?))
                        .context("send pointer datagram")?;
                }
            }
            _ = heartbeat.tick() => {
                heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                write_reliable(&mut stream, &ReliableEvent::Heartbeat { sequence: heartbeat_sequence }).await?;
            }
            _ = tokio::signal::ctrl_c() => {
                let _ = write_reliable(&mut stream, &ReliableEvent::ReleaseAll).await;
                stream.finish()?;
                connection.close(0_u32.into(), b"sender stopped");
                return Ok(());
            }
        }
    }
}

async fn receive(bind: SocketAddr, cert: PathBuf, key: PathBuf) -> Result<()> {
    let endpoint = transport::server_endpoint(bind, &cert, &key)?;
    tracing::info!(local = %endpoint.local_addr()?, "receiver listening");
    loop {
        let connection = transport::accept_one(&endpoint).await?;
        tracing::info!(remote = %connection.remote_address(), "sender connected");
        tokio::select! {
            result = receive_connection(connection.clone()) => {
                if let Err(error) = result {
                    tracing::warn!(%error, "sender disconnected");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                connection.close(0_u32.into(), b"receiver stopped");
                return Ok(());
            }
        }
    }
}

async fn receive_connection(connection: Connection) -> Result<()> {
    let mut injector = Injector::new()?;
    let mut stream = connection
        .accept_uni()
        .await
        .context("accept reliable input stream")?;
    match read_reliable(&mut stream).await? {
        ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
        } => {}
        ReliableEvent::Hello { version } => bail!("unsupported protocol version {version}"),
        _ => bail!("first reliable message must be Hello"),
    }

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
                        ReliableEvent::Input { event_type, code, value, .. } => {
                            pressed.observe(event_type, code, value);
                            injector.emit_raw(event_type, code, value)?;
                        }
                        ReliableEvent::ReleaseAll => release_all(&mut injector, &mut pressed)?,
                        ReliableEvent::Heartbeat { .. } => {}
                        ReliableEvent::Hello { .. } => bail!("unexpected duplicate Hello"),
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
    fn host_command_parses_with_defaults() {
        let cli = Cli::try_parse_from(["rflow", "host"]).unwrap();
        match cli.command {
            Command::Host { bind, cert, key } => {
                assert_eq!(bind, "[::]:24801".parse().unwrap());
                assert_eq!(cert, PathBuf::from("rflow-cert.der"));
                assert_eq!(key, PathBuf::from("rflow-key.der"));
            }
            _ => panic!("expected host command"),
        }
    }

    #[test]
    fn connect_uses_positional_target() {
        let cli = Cli::try_parse_from([
            "rflow",
            "connect",
            "192.168.1.50:24801",
            "--device",
            "/dev/input/event1",
        ])
        .unwrap();
        match cli.command {
            Command::Connect { target, device, .. } => {
                assert_eq!(target, "192.168.1.50:24801".parse().unwrap());
                assert_eq!(device, vec![PathBuf::from("/dev/input/event1")]);
            }
            _ => panic!("expected connect command"),
        }
    }

    #[test]
    fn legacy_commands_are_not_exposed() {
        assert!(Cli::try_parse_from(["rflow", "send"]).is_err());
        assert!(Cli::try_parse_from(["rflow", "receive"]).is_err());
    }
}
