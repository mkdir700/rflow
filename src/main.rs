use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use rflow::{
    core::ScreenSize,
    runtime::{
        AppCommand, AppEvent, ClientConfig, HostConfig, RuntimeHandle, RuntimeStatus,
        TracingDiagnostics,
    },
    transport,
};

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
        /// Keep reconnecting for this many seconds after startup.
        #[arg(long, default_value_t = 0)]
        retry_for: u64,
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
        command => run_session(command).await,
    }
}

async fn run_session(command: Command) -> Result<()> {
    let app_command = match command {
        Command::Host {
            bind,
            cert,
            key,
            size,
            device,
            right,
        } => AppCommand::StartHost(HostConfig {
            bind,
            cert,
            key,
            size,
            devices: device,
            right,
        }),
        Command::Client {
            target,
            cert,
            size,
            retry_for,
        } => AppCommand::StartClient(ClientConfig {
            target,
            cert,
            size,
            retry_for: Duration::from_secs(retry_for),
        }),
        Command::Keygen { .. } => unreachable!("keygen is handled before session startup"),
    };

    let mut runtime = RuntimeHandle::spawn(Arc::new(TracingDiagnostics))?;
    runtime.send(app_command).await?;
    let mut fault = None;
    loop {
        tokio::select! {
            event = runtime.next_event() => match event {
                Some(AppEvent::StatusChanged(status)) => {
                    tracing::info!(?status, "runtime status changed");
                    if status == RuntimeStatus::Stopped {
                        break;
                    }
                }
                Some(AppEvent::PeerChanged(peer)) => tracing::info!(?peer, "peer changed"),
                Some(AppEvent::ControlChanged(control)) => {
                    tracing::info!(?control, "input control changed")
                }
                Some(AppEvent::ConfigChanged(_)) => {}
                Some(AppEvent::Faulted(error)) => {
                    fault = Some(error);
                    break;
                }
                None => bail!("rflow runtime stopped without a terminal event"),
            },
            signal = tokio::signal::ctrl_c() => {
                signal?;
                runtime.send(AppCommand::Stop).await?;
            }
        }
    }
    runtime.shutdown().await?;
    if let Some(fault) = fault {
        bail!("{}", fault.message);
    }
    Ok(())
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
                assert_eq!(size, ScreenSize::new(1920, 1080).unwrap());
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
            Command::Client {
                target,
                size,
                retry_for,
                ..
            } => {
                assert_eq!(target, "192.168.1.50:24801".parse().unwrap());
                assert_eq!(size, None);
                assert_eq!(retry_for, 0);
            }
            _ => panic!("expected client command"),
        }
    }

    #[test]
    fn client_parses_retry_window() {
        let cli = Cli::try_parse_from([
            "rflow",
            "client",
            "192.168.1.50:24801",
            "--retry-for",
            "120",
        ])
        .unwrap();
        match cli.command {
            Command::Client { retry_for, .. } => assert_eq!(retry_for, 120),
            _ => panic!("expected client command"),
        }
    }

    #[test]
    fn retired_connect_command_is_not_exposed() {
        assert!(Cli::try_parse_from(["rflow", "connect", "192.168.1.50:24801"]).is_err());
    }
}
