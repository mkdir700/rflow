use std::{net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rflow::{
    core::{ScreenDirection, ScreenSize},
    identity::{ensure_identity, resolve_identity_paths},
    runtime::{
        AppCommand, AppEvent, ClientConfig, HostConfig, RuntimeHandle, RuntimeStatus,
        TracingDiagnostics,
    },
    target::ServerTarget,
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
        #[arg(long = "identity-cert", requires = "key")]
        cert: Option<PathBuf>,
        #[arg(long = "identity-key", requires = "cert")]
        key: Option<PathBuf>,
        /// Size of this screen in logical cursor coordinates.
        #[arg(long)]
        size: ScreenSize,
        /// Physical Linux evdev path. Repeat for keyboard and mouse.
        #[arg(long)]
        device: Vec<PathBuf>,
        /// Place the client in one of eight directions relative to the host.
        #[arg(long, value_name = "DIRECTION")]
        direction: Option<ScreenDirection>,
    },
    /// Join a host as a remotely controlled screen.
    Client {
        /// Host IP address or hostname, with optional port.
        target: ServerTarget,
        #[arg(long = "identity-cert", requires = "identity_key")]
        identity_cert: Option<PathBuf>,
        #[arg(long = "identity-key", requires = "identity_cert")]
        identity_key: Option<PathBuf>,
        /// Explicitly pin the server certificate until interactive pairing is available.
        #[arg(long)]
        server_cert: Option<PathBuf>,
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
    let app_command = command.into_app_command()?;

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

impl Command {
    fn into_app_command(self) -> Result<AppCommand> {
        Ok(match self {
            Command::Host {
                bind,
                cert,
                key,
                size,
                device,
                direction,
            } => {
                let identity = resolve_identity_paths(cert, key)?;
                ensure_identity(&identity)?;
                AppCommand::StartHost(HostConfig {
                    bind,
                    cert: identity.certificate,
                    key: identity.private_key,
                    size,
                    devices: device,
                    direction,
                })
            }
            Command::Client {
                target,
                identity_cert,
                identity_key,
                server_cert,
                size,
                retry_for,
            } => {
                let server_cert = server_cert.context(
                    "--server-cert is required until interactive device pairing is implemented",
                )?;
                let identity = resolve_identity_paths(identity_cert, identity_key)?;
                ensure_identity(&identity)?;
                AppCommand::StartClient(ClientConfig {
                    target,
                    identity_cert: identity.certificate,
                    identity_key: identity.private_key,
                    server_cert,
                    size,
                    retry_for: Duration::from_secs(retry_for),
                })
            }
            Command::Keygen { .. } => unreachable!("keygen is handled before session startup"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_parses_all_direction_names() {
        let names = [
            "top",
            "top-right",
            "right",
            "bottom-right",
            "bottom",
            "bottom-left",
            "left",
            "top-left",
        ];
        for name in names {
            let cli = Cli::try_parse_from([
                "rflow",
                "host",
                "--size",
                "1920x1080",
                "--direction",
                name,
                "--device",
                "/dev/input/event1",
            ])
            .unwrap();
            let Command::Host { direction, .. } = cli.command else {
                panic!("expected host command")
            };
            assert_eq!(direction.unwrap().to_string(), name);
        }
    }

    #[test]
    fn host_defaults_to_automatic_identity() {
        let cli = Cli::try_parse_from([
            "rflow",
            "host",
            "--size",
            "1920x1080",
            "--direction",
            "right",
        ])
        .unwrap();
        let Command::Host { cert, key, .. } = cli.command else {
            panic!("expected host command")
        };
        assert_eq!(cert, None);
        assert_eq!(key, None);
    }

    #[test]
    fn client_separates_local_identity_from_server_trust_anchor() {
        let cli = Cli::try_parse_from([
            "rflow",
            "client",
            "desktop.local",
            "--identity-cert",
            "client-cert.der",
            "--identity-key",
            "client-key.der",
            "--server-cert",
            "server-cert.der",
        ])
        .unwrap();
        let Command::Client {
            identity_cert,
            identity_key,
            server_cert,
            ..
        } = cli.command
        else {
            panic!("expected client command")
        };
        assert_eq!(identity_cert, Some(PathBuf::from("client-cert.der")));
        assert_eq!(identity_key, Some(PathBuf::from("client-key.der")));
        assert_eq!(server_cert, Some(PathBuf::from("server-cert.der")));
    }

    #[test]
    fn identity_override_requires_certificate_and_key_together() {
        assert!(
            Cli::try_parse_from([
                "rflow",
                "host",
                "--size",
                "1920x1080",
                "--direction",
                "right",
                "--identity-cert",
                "identity-cert.der",
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "rflow",
                "client",
                "desktop.local",
                "--identity-key",
                "identity-key.der",
            ])
            .is_err()
        );
    }

    #[test]
    fn host_rejects_retired_right_option() {
        assert!(Cli::try_parse_from(["rflow", "host", "--size", "1920x1080", "--right"]).is_err());
    }

    #[test]
    fn host_rejects_unknown_direction() {
        assert!(
            Cli::try_parse_from([
                "rflow",
                "host",
                "--size",
                "1920x1080",
                "--direction",
                "upper-right",
            ])
            .is_err()
        );
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
    fn client_accepts_hostname_without_port() {
        let cli = Cli::try_parse_from(["rflow", "client", "linux-desktop.local"]).unwrap();
        let Command::Client { target, .. } = cli.command else {
            panic!("expected client command")
        };
        assert_eq!(target.to_string(), "linux-desktop.local:24801");
    }

    #[test]
    fn client_accepts_ip_without_port() {
        let cli = Cli::try_parse_from(["rflow", "client", "192.168.1.50"]).unwrap();
        let Command::Client { target, .. } = cli.command else {
            panic!("expected client command")
        };
        assert_eq!(target.to_string(), "192.168.1.50:24801");
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
