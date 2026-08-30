use std::{
    io::{self, IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use rflow::{
    core::{ScreenDirection, ScreenSize},
    identity::{device_display_name, ensure_identity, resolve_identity_paths},
    management::{
        ManagementRequest, ManagementResponse, ManagementServer, default_endpoint_path,
        request as management_request,
    },
    pairing::PairingRequestId,
    runtime::{
        AppCommand, AppEvent, ClientConfig, HostConfig, RuntimeHandle, RuntimeStatus,
        TracingDiagnostics,
    },
    target::ServerTarget,
    transport,
    trust::default_trust_store_path,
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
        /// Override automatic logical screen-size detection.
        #[arg(long)]
        size: Option<ScreenSize>,
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
        /// Explicitly pin the expected server certificate instead of relying only on pairing.
        #[arg(long)]
        server_cert: Option<PathBuf>,
        /// Override automatic screen-size detection.
        #[arg(long)]
        size: Option<ScreenSize>,
        /// Keep reconnecting for this many seconds after startup.
        #[arg(long, default_value_t = 0)]
        retry_for: u64,
    },
    /// List and manage trusted devices.
    Peers {
        #[command(subcommand)]
        command: Option<PeersCommand>,
    },
    /// Render the authoritative screen topology.
    Layout {
        /// Redraw whenever the topology revision changes.
        #[arg(long)]
        watch: bool,
        /// Emit the versioned machine-readable schema.
        #[arg(long, conflicts_with = "watch")]
        json: bool,
        /// Replace the topology from a JSON file through the running host.
        #[arg(long, value_name = "FILE", conflicts_with_all = ["watch", "json"], requires = "expected_revision")]
        apply: Option<PathBuf>,
        /// Revision observed before editing; prevents lost updates.
        #[arg(long, requires = "apply")]
        expected_revision: Option<u64>,
    },
}

#[derive(Debug, Subcommand)]
enum PeersCommand {
    /// Show the active host's pending pairing request.
    Pending,
    /// Accept a pending pairing request.
    Accept { request_id: PairingRequestId },
    /// Reject a pending pairing request.
    Reject { request_id: PairingRequestId },
    /// Remove a trusted device and its endpoint bindings.
    Forget {
        device: String,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
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
        Command::Peers { command } => run_peers(command).await,
        Command::Layout {
            watch,
            json,
            apply,
            expected_revision,
        } => run_layout(watch, json, apply, expected_revision).await,
        command => run_session(command).await,
    }
}

async fn run_layout(
    watch: bool,
    json: bool,
    apply: Option<PathBuf>,
    expected_revision: Option<u64>,
) -> Result<()> {
    let path = default_endpoint_path()?;
    if let Some(file) = apply {
        let topology: rflow::core::ScreenTopology =
            serde_json::from_slice(&std::fs::read(&file).map_err(anyhow::Error::from)?)?;
        topology.validate().map_err(anyhow::Error::msg)?;
        return match management_request(
            &path,
            ManagementRequest::ReplaceTopology {
                expected_revision: expected_revision.expect("clap requires the revision"),
                topology,
            },
        )
        .await?
        {
            ManagementResponse::TopologyQueued => {
                println!("Topology update queued.");
                Ok(())
            }
            ManagementResponse::Error(message) => bail!("{message}"),
            _ => bail!("rflow host returned an invalid management response"),
        };
    }
    let mut last_revision = None;
    loop {
        let topology = match management_request(&path, ManagementRequest::Layout).await? {
            ManagementResponse::Layout(topology) => topology,
            ManagementResponse::Error(message) => bail!("{message}"),
            _ => bail!("rflow host returned an invalid management response"),
        };
        if last_revision != Some(topology.revision) {
            if watch && last_revision.is_some() && io::stdout().is_terminal() {
                print!("\x1b[2J\x1b[H");
            }
            println!(
                "{}",
                if json {
                    rflow::layout::render_json(&topology)?
                } else {
                    rflow::layout::render_text(&topology)
                }
            );
            io::stdout().flush()?;
            last_revision = Some(topology.revision);
        }
        if !watch {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn run_peers(command: Option<PeersCommand>) -> Result<()> {
    match command {
        None => {
            let trust = rflow::trust::TrustStore::platform_default()?;
            if trust.peers().is_empty() {
                println!("No trusted devices.");
            } else {
                println!("TRUSTED DEVICES");
                for peer in trust.peers() {
                    println!("{}  {}", peer.device_id, peer.display_name);
                }
            }
        }
        Some(PeersCommand::Pending) => {
            match management_request(default_endpoint_path()?, ManagementRequest::Pending).await? {
                ManagementResponse::Pending(Some(request)) => println!(
                    "PENDING PAIRING\nRequest: {}\nDevice: {}\nAddress: {}\nFingerprint: {}\nPairing code: {}\nExpires in: {}s\nExpires at (Unix): {}",
                    request.request_id,
                    request.device_name,
                    request.address,
                    request.fingerprint,
                    request.code,
                    request.expires_in_seconds,
                    request.expires_at_unix_seconds,
                ),
                ManagementResponse::Pending(None) => println!("No pending pairing requests."),
                ManagementResponse::Error(message) => bail!("{message}"),
                ManagementResponse::DecisionQueued => {
                    bail!("rflow host returned an invalid management response")
                }
                ManagementResponse::Layout(_) => {
                    bail!("rflow host returned an invalid management response")
                }
                ManagementResponse::TopologyQueued => {
                    bail!("rflow host returned an invalid management response")
                }
            }
        }
        Some(PeersCommand::Accept { request_id }) => {
            submit_pairing_decision(request_id, true).await?;
            println!("Accepted {request_id}.");
        }
        Some(PeersCommand::Reject { request_id }) => {
            submit_pairing_decision(request_id, false).await?;
            println!("Rejected {request_id}.");
        }
        Some(PeersCommand::Forget { device, yes }) => {
            let mut trust = rflow::trust::TrustStore::platform_default()?;
            let device_id = trust.resolve_peer(&device)?;
            if !yes {
                print!("Forget trusted device {device} ({device_id})? [y/N] ");
                io::stdout().flush()?;
                let mut answer = String::new();
                let confirmed = io::stdin().read_line(&mut answer).is_ok()
                    && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                if !confirmed {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            trust.forget(device_id)?;
            println!("Forgot {device}.");
        }
    }
    Ok(())
}

async fn submit_pairing_decision(request_id: PairingRequestId, accepted: bool) -> Result<()> {
    let request = if accepted {
        ManagementRequest::Accept(request_id.0)
    } else {
        ManagementRequest::Reject(request_id.0)
    };
    match management_request(default_endpoint_path()?, request).await? {
        ManagementResponse::DecisionQueued => Ok(()),
        ManagementResponse::Error(message) => bail!("{message}"),
        ManagementResponse::Pending(_) => {
            bail!("rflow host returned an invalid management response")
        }
        ManagementResponse::Layout(_) => {
            bail!("rflow host returned an invalid management response")
        }
        ManagementResponse::TopologyQueued => {
            bail!("rflow host returned an invalid management response")
        }
    }
}

async fn run_session(command: Command) -> Result<()> {
    let app_command = command.into_app_command()?;
    let is_host = matches!(app_command, AppCommand::StartHost(_));
    let (local_screen, mut remote_screen) = match &app_command {
        AppCommand::StartHost(config) => (config.device_name.clone(), "remote-device".to_owned()),
        AppCommand::StartClient(config) => (config.device_name.clone(), config.target.to_string()),
        _ => unreachable!("session startup requires host or client command"),
    };

    let mut runtime = RuntimeHandle::spawn(Arc::new(TracingDiagnostics))?;
    let management = if is_host {
        match ManagementServer::bind(default_endpoint_path()?, runtime.application_handle()).await {
            Ok(management) => Some(management),
            Err(error) => {
                runtime.shutdown().await?;
                return Err(error);
            }
        }
    } else {
        None
    };
    runtime.send(app_command).await?;
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::channel(4);
    let mut fault = None;
    let mut previous_control = None;
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
                Some(AppEvent::PeerIdentified(peer)) => {
                    remote_screen = peer.display_name;
                }
                Some(AppEvent::ControlChanged(control)) => {
                    if let Some(transition) = screen_transition(
                        previous_control,
                        control,
                        &local_screen,
                        &remote_screen,
                    ) {
                        match transition.action {
                            "enter" => tracing::info!(from = %transition.from, to = %transition.to, "enter screen"),
                            "leave" => tracing::info!(from = %transition.from, to = %transition.to, "leave screen"),
                            _ => unreachable!(),
                        }
                    }
                    previous_control = Some(control);
                }
                Some(AppEvent::ConfigChanged(_)) => {}
                Some(AppEvent::PairingRequested(request)) => {
                    println!(
                        "\nPairing request\n\nRequest: {}\nDevice: {}\nAddress: {}\nFingerprint: {}\nPairing code: {}\n",
                        request.request_id,
                        request.device_name,
                        request.address,
                        request.fingerprint,
                        request.code,
                    );
                    if io::stdin().is_terminal() {
                        print!("Accept this device? [y/N] ");
                        io::stdout().flush()?;
                        let prompt_tx = prompt_tx.clone();
                        let request_id = request.request_id;
                        std::thread::Builder::new()
                            .name("rflow-pairing-prompt".to_owned())
                            .spawn(move || {
                                let mut answer = String::new();
                                let accepted = io::stdin().read_line(&mut answer).is_ok()
                                    && matches!(
                                        answer.trim().to_ascii_lowercase().as_str(),
                                        "y" | "yes"
                                    );
                                let _ = prompt_tx.blocking_send((request_id, accepted));
                            })?;
                    } else {
                        println!(
                            "No interactive terminal; decide with `rflow peers accept {}` or `rflow peers reject {}`.",
                            request.request_id, request.request_id
                        );
                    }
                }
                Some(AppEvent::PairingCodeReady(request)) => println!(
                    "\nUntrusted server\nDevice: {}\nAddress: {}\nFingerprint: {}\nPairing code: {}\n\nWaiting for confirmation on the server...",
                    request.device_name, request.address, request.fingerprint, request.code,
                ),
                Some(AppEvent::PairingCleared(_)) => {}
                Some(AppEvent::PairingExpired(request_id)) => {
                    println!("Pairing request {request_id} expired.");
                }
                Some(AppEvent::PeerTrusted(peer)) => {
                    tracing::info!(device_id = %peer.device_id, name = %peer.display_name, "peer trusted");
                }
                Some(AppEvent::TopologyChanged(_)) => {}
                Some(AppEvent::Faulted(error)) => {
                    fault = Some(error);
                    break;
                }
                None => bail!("rflow runtime stopped without a terminal event"),
            },
            Some((request_id, accepted)) = prompt_rx.recv() => {
                if runtime
                    .snapshot()
                    .pairing
                    .as_ref()
                    .is_some_and(|request| request.request_id == request_id)
                {
                    runtime
                        .send(if accepted {
                            AppCommand::AcceptPairing(request_id)
                        } else {
                            AppCommand::RejectPairing(request_id)
                        })
                        .await?;
                }
            }
            signal = tokio::signal::ctrl_c() => {
                signal?;
                runtime.send(AppCommand::Stop).await?;
            }
        }
    }
    if let Some(management) = management {
        management.shutdown().await?;
    }
    runtime.shutdown().await?;
    if let Some(fault) = fault {
        bail!("{}", fault.message);
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
struct ScreenTransition<'a> {
    action: &'static str,
    from: &'a str,
    to: &'a str,
}

fn screen_transition<'a>(
    previous: Option<rflow::core::ControlTarget>,
    current: rflow::core::ControlTarget,
    local: &'a str,
    remote: &'a str,
) -> Option<ScreenTransition<'a>> {
    match (previous, current) {
        (Some(rflow::core::ControlTarget::Local), rflow::core::ControlTarget::Remote) => {
            Some(ScreenTransition {
                action: "leave",
                from: local,
                to: remote,
            })
        }
        (Some(rflow::core::ControlTarget::Remote), rflow::core::ControlTarget::Local) => {
            Some(ScreenTransition {
                action: "enter",
                from: remote,
                to: local,
            })
        }
        _ => None,
    }
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
                    device_name: device_display_name(),
                    trust_store: default_trust_store_path()?,
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
                let identity = resolve_identity_paths(identity_cert, identity_key)?;
                ensure_identity(&identity)?;
                AppCommand::StartClient(ClientConfig {
                    target,
                    identity_cert: identity.certificate,
                    identity_key: identity.private_key,
                    server_cert,
                    size,
                    retry_for: Duration::from_secs(retry_for),
                    device_name: device_display_name(),
                    trust_store: default_trust_store_path()?,
                })
            }
            Command::Keygen { .. } => unreachable!("keygen is handled before session startup"),
            Command::Peers { .. } => {
                unreachable!("peer management is handled before session startup")
            }
            Command::Layout { .. } => {
                unreachable!("layout is handled before session startup")
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_transitions_name_both_screens_and_boundary_action() {
        assert_eq!(
            screen_transition(
                Some(rflow::core::ControlTarget::Local),
                rflow::core::ControlTarget::Remote,
                "linux-desktop",
                "macmini",
            ),
            Some(ScreenTransition {
                action: "leave",
                from: "linux-desktop",
                to: "macmini",
            })
        );
        assert_eq!(
            screen_transition(
                Some(rflow::core::ControlTarget::Remote),
                rflow::core::ControlTarget::Local,
                "linux-desktop",
                "macmini",
            ),
            Some(ScreenTransition {
                action: "enter",
                from: "macmini",
                to: "linux-desktop",
            })
        );
        assert_eq!(
            screen_transition(
                None,
                rflow::core::ControlTarget::Local,
                "linux-desktop",
                "macmini",
            ),
            None
        );
    }

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
    fn host_defaults_to_automatic_screen_and_devices() {
        let cli = Cli::try_parse_from(["rflow", "host", "--direction", "right"]).unwrap();
        let Command::Host { size, device, .. } = cli.command else {
            panic!("expected host command")
        };
        assert_eq!(size, None);
        assert!(device.is_empty());
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
    fn peers_forget_requires_an_explicit_device_and_supports_yes() {
        let cli = Cli::try_parse_from(["rflow", "peers", "forget", "macmini", "--yes"]).unwrap();
        let Command::Peers {
            command: Some(PeersCommand::Forget { device, yes }),
        } = cli.command
        else {
            panic!("expected peers forget command")
        };
        assert_eq!(device, "macmini");
        assert!(yes);
    }

    #[test]
    fn peers_management_commands_parse_request_ids() {
        let cli = Cli::try_parse_from(["rflow", "peers", "pending"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Peers {
                command: Some(PeersCommand::Pending)
            }
        ));

        let cli = Cli::try_parse_from(["rflow", "peers", "accept", "p-000000000000002a"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Peers {
                command: Some(PeersCommand::Accept {
                    request_id: PairingRequestId(42)
                })
            }
        ));
        assert!(Cli::try_parse_from(["rflow", "peers", "reject", "42"]).is_err());
    }

    #[test]
    fn retired_connect_command_is_not_exposed() {
        assert!(Cli::try_parse_from(["rflow", "connect", "192.168.1.50:24801"]).is_err());
    }
}
