use std::{
    io::{self, BufRead, IsTerminal, Write},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use clap::{ArgGroup, Parser, Subcommand};
use rflow::{
    core::{
        Edge, LayoutCommand, RelativePosition, ScreenDirection, ScreenEdge, ScreenId, ScreenLayout,
        ScreenSize, ScreenTopology,
    },
    identity::{device_display_name, ensure_identity, resolve_identity_paths},
    management::{
        ManagementRequest, ManagementResponse, ManagementServer, default_endpoint_path,
        request as management_request,
    },
    pairing::PairingRequestId,
    runtime::{
        AppCommand, AppEvent, ClientConfig, HostConfig, PlacementRequestSummary, RuntimeHandle,
        RuntimeStatus, TracingDiagnostics,
    },
    target::ServerTarget,
    transport,
    trust::default_trust_store_path,
};

#[cfg(test)]
use rflow::runtime::{PlacementOptionSummary, PlacementScreenSummary};

#[derive(Debug, Parser)]
#[command(version, about = "Low-latency virtual KVM over QUIC")]
struct Cli {
    /// Increase runtime log detail (-v for debug, -vv for trace).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    verbose: u8,

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
        #[arg(long, default_value = "0.0.0.0:24801")]
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
        /// One-time migration aid for an unconfigured single-screen peer (cardinal directions only).
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
        /// Keep reconnecting for this many seconds after startup, or forever.
        #[arg(long, default_value_t = RetryFor::Duration(Duration::ZERO))]
        retry_for: RetryFor,
    },
    /// List and manage trusted devices.
    Peers {
        #[command(subcommand)]
        command: Option<PeersCommand>,
    },
    /// Render the authoritative screen topology.
    Layout {
        #[command(subcommand)]
        command: Option<LayoutSubcommand>,
        /// Redraw whenever the topology revision changes.
        #[arg(long)]
        watch: bool,
        /// Emit the versioned machine-readable schema.
        #[arg(long, conflicts_with = "watch")]
        json: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetryFor {
    Duration(Duration),
    Forever,
}

impl std::fmt::Display for RetryFor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duration(duration) => write!(formatter, "{}", duration.as_secs()),
            Self::Forever => formatter.write_str("forever"),
        }
    }
}

impl std::str::FromStr for RetryFor {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.eq_ignore_ascii_case("forever") {
            return Ok(Self::Forever);
        }

        value
            .parse::<u64>()
            .map(|seconds| Self::Duration(Duration::from_secs(seconds)))
            .map_err(|_| "expected a number of seconds or 'forever'".to_owned())
    }
}

#[derive(Debug, Subcommand)]
enum LayoutSubcommand {
    /// List known screens and their current placement status.
    Screens,
    /// Place one screen relative to another screen.
    #[command(group(ArgGroup::new("position").required(true).multiple(false).args(["left_of", "right_of", "above", "below"])))]
    Place {
        screen: String,
        #[arg(long)]
        left_of: Option<String>,
        #[arg(long)]
        right_of: Option<String>,
        #[arg(long)]
        above: Option<String>,
        #[arg(long)]
        below: Option<String>,
        #[arg(long)]
        replace: bool,
    },
    /// Link two exact screen edges.
    Link {
        from: EdgeSpec,
        to: EdgeSpec,
        #[arg(long)]
        replace: bool,
    },
    /// Remove the link attached to one screen edge.
    Unlink { edge: EdgeSpec },
    /// Remove every link attached to a screen.
    Unplace { screen: String },
    /// Override the logical size used for coordinate mapping.
    SetSize { screen: String, size: ScreenSize },
    /// Clear a logical size override.
    ClearSize { screen: String },
    /// Export the persistent user layout document.
    Export,
    /// Replace the persistent user layout document.
    Apply {
        file: PathBuf,
        #[arg(long)]
        expected_revision: u64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EdgeSpec {
    screen: String,
    edge: Edge,
}

impl std::str::FromStr for EdgeSpec {
    type Err = String;

    fn from_str(value: &str) -> std::result::Result<Self, Self::Err> {
        let (screen, edge) = value
            .rsplit_once('.')
            .ok_or_else(|| "screen edge must use SCREEN.EDGE syntax".to_owned())?;
        if screen.trim().is_empty() {
            return Err("screen edge must name a screen".to_owned());
        }
        Ok(Self {
            screen: screen.to_owned(),
            edge: edge.parse()?,
        })
    }
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
        #[arg(required_unless_present = "all", conflicts_with = "all")]
        device: Option<String>,
        /// Remove every trusted device and endpoint binding.
        #[arg(long)]
        all: bool,
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    tracing_subscriber::fmt()
        .with_env_filter(runtime_log_filter(cli.verbose))
        .init();
    tracing::debug!(verbosity = cli.verbose, "runtime logging enabled");
    match cli.command {
        Command::Keygen { cert, key, force } => {
            transport::generate_identity(&cert, &key, force)?;
            println!("wrote {} and {}", cert.display(), key.display());
            Ok(())
        }
        Command::Peers { command } => run_peers(command).await,
        Command::Layout {
            command,
            watch,
            json,
        } => run_layout(command, watch, json).await,
        command => run_session(command).await,
    }
}

fn runtime_log_filter(verbose: u8) -> tracing_subscriber::EnvFilter {
    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        tracing_subscriber::EnvFilter::new(match verbose {
            0 => "error",
            1 => "rflow=debug",
            _ => "rflow=trace",
        })
    })
}

async fn run_layout(command: Option<LayoutSubcommand>, watch: bool, json: bool) -> Result<()> {
    let path = default_endpoint_path()?;
    let topology_path = rflow::topology_store::default_path()?;
    if let Some(command) = command {
        let topology = request_topology(&path, &topology_path).await?;
        return run_layout_command(&path, &topology_path, topology, command).await;
    }
    let mut last_revision = None;
    loop {
        let topology = request_topology(&path, &topology_path).await?;
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

async fn request_topology(
    path: &std::path::Path,
    topology_path: &std::path::Path,
) -> Result<ScreenTopology> {
    match management_request(path, ManagementRequest::Layout).await {
        Ok(ManagementResponse::Layout(topology)) => Ok(topology),
        Ok(ManagementResponse::Error(message)) => bail!("{message}"),
        Ok(_) => bail!("rflow host returned an invalid management response"),
        Err(error) if management_endpoint_unavailable(&error) => {
            Ok(rflow::topology_store::load(topology_path)?.unwrap_or_default())
        }
        Err(error) => Err(error),
    }
}

fn management_endpoint_unavailable(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause.downcast_ref::<io::Error>().is_some_and(|error| {
            matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            )
        })
    })
}

async fn run_layout_command(
    path: &std::path::Path,
    topology_path: &std::path::Path,
    topology: ScreenTopology,
    command: LayoutSubcommand,
) -> Result<()> {
    match command {
        LayoutSubcommand::Screens => {
            print_screens(&topology);
            Ok(())
        }
        LayoutSubcommand::Export => {
            println!(
                "{}",
                serde_json::to_string_pretty(&ScreenLayout::from_topology(&topology))?
            );
            Ok(())
        }
        LayoutSubcommand::Apply {
            file,
            expected_revision,
        } => {
            let layout: ScreenLayout = serde_json::from_slice(&std::fs::read(file)?)?;
            submit_layout_command(
                path,
                topology_path,
                expected_revision,
                LayoutCommand::Replace { layout },
            )
            .await
        }
        LayoutSubcommand::Place {
            screen,
            left_of,
            right_of,
            above,
            below,
            replace,
        } => {
            let screen_id = resolve_screen_id(&topology, &screen)?;
            let (anchor, position) = if let Some(anchor) = left_of {
                (anchor, RelativePosition::LeftOf)
            } else if let Some(anchor) = right_of {
                (anchor, RelativePosition::RightOf)
            } else if let Some(anchor) = above {
                (anchor, RelativePosition::Above)
            } else if let Some(anchor) = below {
                (anchor, RelativePosition::Below)
            } else {
                unreachable!("clap requires one relative position")
            };
            let anchor_id = resolve_screen_id(&topology, &anchor)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::Place {
                    screen_id,
                    anchor_id,
                    position,
                    replace,
                },
            )
            .await
        }
        LayoutSubcommand::Link { from, to, replace } => {
            let from = resolve_edge(&topology, from)?;
            let to = resolve_edge(&topology, to)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::Link { from, to, replace },
            )
            .await
        }
        LayoutSubcommand::Unlink { edge } => {
            let edge = resolve_edge(&topology, edge)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::Unlink { edge },
            )
            .await
        }
        LayoutSubcommand::Unplace { screen } => {
            let screen_id = resolve_screen_id(&topology, &screen)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::Unplace { screen_id },
            )
            .await
        }
        LayoutSubcommand::SetSize { screen, size } => {
            let screen_id = resolve_screen_id(&topology, &screen)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::SetSizeOverride {
                    screen_id,
                    size: Some(size),
                },
            )
            .await
        }
        LayoutSubcommand::ClearSize { screen } => {
            let screen_id = resolve_screen_id(&topology, &screen)?;
            submit_layout_command(
                path,
                topology_path,
                topology.revision,
                LayoutCommand::SetSizeOverride {
                    screen_id,
                    size: None,
                },
            )
            .await
        }
    }
}

async fn submit_layout_command(
    path: &std::path::Path,
    topology_path: &std::path::Path,
    expected_revision: u64,
    command: LayoutCommand,
) -> Result<()> {
    let response = management_request(
        path,
        ManagementRequest::ApplyLayout {
            expected_revision,
            command: command.clone(),
        },
    )
    .await;
    match response {
        Ok(ManagementResponse::LayoutUpdated(topology)) => {
            println!("Layout updated to revision {}.", topology.revision);
            Ok(())
        }
        Ok(ManagementResponse::Error(message)) => bail!("{message}"),
        Ok(_) => bail!("rflow host returned an invalid management response"),
        Err(error) if management_endpoint_unavailable(&error) => {
            let state = rflow::topology_store::load_state(topology_path)?;
            let topology = rflow::topology_store::apply(
                topology_path,
                expected_revision,
                &state.inventory,
                command,
            )?;
            println!(
                "Layout updated to revision {}. It will take effect when rflow host starts.",
                topology.revision
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

fn resolve_edge(topology: &ScreenTopology, value: EdgeSpec) -> Result<ScreenEdge> {
    Ok(ScreenEdge {
        screen_id: resolve_screen_id(topology, &value.screen)?,
        edge: value.edge,
    })
}

fn resolve_screen_id(topology: &ScreenTopology, query: &str) -> Result<ScreenId> {
    if let Some(screen) = topology
        .screens
        .iter()
        .find(|screen| screen.screen_id.0 == query)
    {
        return Ok(screen.screen_id.clone());
    }
    let matches: Vec<_> = topology
        .screens
        .iter()
        .filter(|screen| {
            screen.device_id.0 == query || screen.device_name == query || screen.name == query
        })
        .collect();
    match matches.as_slice() {
        [screen] => Ok(screen.screen_id.clone()),
        [] => bail!("screen or device {query:?} was not found"),
        matches => bail!(
            "device or screen name {query:?} matches multiple screens:\n{}\nSpecify a screen ID explicitly.",
            matches
                .iter()
                .map(|screen| format!("  {}", screen.screen_id.0))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    }
}

fn print_screens(topology: &ScreenTopology) {
    println!("SCREEN\tDEVICE\tSIZE\tSTATUS\tPLACEMENT");
    for screen in &topology.screens {
        let placement = if screen.this_device {
            "local"
        } else if topology.links.iter().any(|link| {
            link.from.screen_id == screen.screen_id || link.to.screen_id == screen.screen_id
        }) {
            "placed"
        } else {
            "unplaced"
        };
        let size = screen.effective_size();
        println!(
            "{}\t{}\t{}x{}\t{}\t{}",
            screen.screen_id.0,
            screen.device_name,
            size.width,
            size.height,
            if screen.online { "online" } else { "offline" },
            placement
        );
    }
}

async fn run_peers(command: Option<PeersCommand>) -> Result<()> {
    match command {
        None => {
            let trust = rflow::trust::TrustStore::platform_default()?;
            if trust.peers().is_empty() {
                println!("No trusted devices.");
            } else {
                let topology =
                    rflow::topology_store::load(&rflow::topology_store::default_path()?)?
                        .unwrap_or_default();
                println!("DEVICE\tTRUSTED\tONLINE\tSCREENS\tPLACEMENT\tFINGERPRINT");
                for peer in trust.peers() {
                    let (online, screens, placement) =
                        peer_layout_summary(&topology, &peer.device_id.to_string());
                    println!(
                        "{}\tyes\t{}\t{}\t{}\t{}",
                        peer.display_name,
                        if online { "yes" } else { "no" },
                        screens,
                        placement,
                        peer.device_id,
                    );
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
                ManagementResponse::LayoutUpdated(_) => {
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
        Some(PeersCommand::Forget { device, all, yes }) => {
            let mut trust = rflow::trust::TrustStore::platform_default()?;
            let peers: Vec<_> = if all {
                trust
                    .peers()
                    .iter()
                    .map(|peer| (peer.device_id, peer.display_name.clone()))
                    .collect()
            } else {
                let device = device.as_deref().expect("clap requires a device or --all");
                let device_id = trust.resolve_peer(device)?;
                vec![(device_id, device.to_owned())]
            };
            if peers.is_empty() {
                println!("No trusted devices.");
                return Ok(());
            }
            if !yes {
                if all {
                    print!("Forget all {} trusted devices? [y/N] ", peers.len());
                } else {
                    print!(
                        "Forget trusted device {} ({})? [y/N] ",
                        peers[0].1, peers[0].0
                    );
                }
                io::stdout().flush()?;
                let mut answer = String::new();
                let confirmed = io::stdin().read_line(&mut answer).is_ok()
                    && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                if !confirmed {
                    println!("Cancelled.");
                    return Ok(());
                }
            }
            if all {
                trust.forget_all()?;
            } else {
                trust.forget(peers[0].0)?;
            }
            let endpoint = default_endpoint_path()?;
            let mut rejected = None;
            for (device_id, _) in &peers {
                if let Ok(ManagementResponse::Error(message)) = management_request(
                    &endpoint,
                    ManagementRequest::ForgetPeer(device_id.to_string()),
                )
                .await
                {
                    rejected.get_or_insert(message);
                }
            }
            if let Some(message) = rejected {
                bail!("peers were forgotten, but the running host rejected an update: {message}");
            }
            if all {
                println!("Forgot {} trusted devices.", peers.len());
            } else {
                println!("Forgot {}.", peers[0].1);
            }
        }
    }
    Ok(())
}

fn peer_layout_summary(topology: &ScreenTopology, device_id: &str) -> (bool, usize, &'static str) {
    let screens: Vec<_> = topology
        .screens
        .iter()
        .filter(|screen| screen.device_id.0 == device_id)
        .collect();
    let placed = screens
        .iter()
        .filter(|screen| {
            topology.links.iter().any(|link| {
                link.from.screen_id == screen.screen_id || link.to.screen_id == screen.screen_id
            })
        })
        .count();
    let placement = match (placed, screens.len()) {
        (0, _) => "unplaced",
        (placed, total) if placed == total => "placed",
        _ => "partially placed",
    };
    (
        screens.iter().any(|screen| screen.online),
        screens.len(),
        placement,
    )
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
        ManagementResponse::LayoutUpdated(_) => {
            bail!("rflow host returned an invalid management response")
        }
    }
}

#[derive(Debug)]
enum TerminalPrompt {
    Pairing(Box<rflow::runtime::PairingRequestSummary>),
    Placement(Box<PlacementRequestSummary>),
}

#[derive(Debug)]
enum TerminalPromptResponse {
    Pairing(PairingRequestId, bool),
    Placement(std::result::Result<Option<PlacementDecision>, String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PlacementDecision {
    revision: u64,
    screen_id: ScreenId,
    anchor_id: ScreenId,
    position: RelativePosition,
}

fn placement_position_name(position: RelativePosition) -> &'static str {
    match position {
        RelativePosition::LeftOf => "Left",
        RelativePosition::RightOf => "Right",
        RelativePosition::Above => "Above",
        RelativePosition::Below => "Below",
    }
}

fn prompt_peer_placement(
    request: &PlacementRequestSummary,
    input: &mut impl BufRead,
    output: &mut impl Write,
) -> io::Result<Option<PlacementDecision>> {
    writeln!(
        output,
        "\nPlace new device {} relative to {}",
        request.device_name, request.anchor_name
    )?;
    let screen_index = if request.screens.len() == 1 {
        0
    } else {
        writeln!(output, "\nScreens")?;
        for (index, screen) in request.screens.iter().enumerate() {
            writeln!(
                output,
                "  {}) {} ({})",
                index + 1,
                screen.name,
                screen.screen_id.0
            )?;
        }
        loop {
            write!(
                output,
                "Choose a screen [1-{}, 0=later]: ",
                request.screens.len()
            )?;
            output.flush()?;
            let choice = match read_prompt_choice(input)? {
                PromptChoice::End => return Ok(None),
                PromptChoice::Invalid => {
                    writeln!(output, "Invalid screen choice.")?;
                    continue;
                }
                PromptChoice::Value(choice) => choice,
            };
            if choice == 0 {
                return Ok(None);
            }
            if (1..=request.screens.len()).contains(&choice) {
                break choice - 1;
            }
            writeln!(output, "Invalid screen choice.")?;
        }
    };
    let screen = &request.screens[screen_index];
    writeln!(output, "\nPosition for {}", screen.name)?;
    for (index, option) in screen.options.iter().enumerate() {
        if option.occupied_by.is_empty() {
            writeln!(
                output,
                "  {}) {}",
                index + 1,
                placement_position_name(option.position)
            )?;
        } else {
            writeln!(
                output,
                "  {}) {} [unavailable: occupied by {}]",
                index + 1,
                placement_position_name(option.position),
                option.occupied_by.join(", ")
            )?;
        }
    }
    loop {
        write!(output, "Choose a position [1-4, 0=later]: ")?;
        output.flush()?;
        let choice = match read_prompt_choice(input)? {
            PromptChoice::End => return Ok(None),
            PromptChoice::Invalid => {
                writeln!(output, "Invalid position choice.")?;
                continue;
            }
            PromptChoice::Value(choice) => choice,
        };
        if choice == 0 {
            return Ok(None);
        }
        let Some(option) = screen.options.get(choice.saturating_sub(1)) else {
            writeln!(output, "Invalid position choice.")?;
            continue;
        };
        if !option.occupied_by.is_empty() {
            writeln!(output, "That position is unavailable.")?;
            continue;
        }
        return Ok(Some(PlacementDecision {
            revision: request.revision,
            screen_id: screen.screen_id.clone(),
            anchor_id: request.anchor_screen.clone(),
            position: option.position,
        }));
    }
}

enum PromptChoice {
    End,
    Invalid,
    Value(usize),
}

fn read_prompt_choice(input: &mut impl BufRead) -> io::Result<PromptChoice> {
    let mut answer = String::new();
    if input.read_line(&mut answer)? == 0 {
        return Ok(PromptChoice::End);
    }
    Ok(match answer.trim().parse() {
        Ok(choice) => PromptChoice::Value(choice),
        Err(_) => PromptChoice::Invalid,
    })
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
    let application = runtime.application_handle();
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
    let interactive = io::stdin().is_terminal();
    let (prompt_request_tx, mut prompt_request_rx) = tokio::sync::mpsc::channel(8);
    let (prompt_response_tx, mut prompt_response_rx) = tokio::sync::mpsc::channel(8);
    if interactive {
        std::thread::Builder::new()
            .name("rflow-terminal-prompts".to_owned())
            .spawn(move || {
                while let Some(prompt) = prompt_request_rx.blocking_recv() {
                    // Do not hold the process-wide terminal locks while waiting
                    // for a prompt. Tracing and Ctrl-C handling run on the main
                    // thread and must remain able to make progress while idle.
                    let stdin = io::stdin();
                    let mut input = stdin.lock();
                    let stdout = io::stdout();
                    let mut output = stdout.lock();
                    let response = match prompt {
                        TerminalPrompt::Pairing(request) => {
                            let _ = writeln!(
                                output,
                                "\nPairing request\n\nRequest: {}\nDevice: {}\nAddress: {}\nFingerprint: {}\nPairing code: {}\n",
                                request.request_id,
                                request.device_name,
                                request.address,
                                request.fingerprint,
                                request.code,
                            );
                            let _ = write!(output, "Accept this device? [y/N] ");
                            let _ = output.flush();
                            let mut answer = String::new();
                            let accepted = input.read_line(&mut answer).is_ok()
                                && matches!(
                                    answer.trim().to_ascii_lowercase().as_str(),
                                    "y" | "yes"
                                );
                            TerminalPromptResponse::Pairing(request.request_id, accepted)
                        }
                        TerminalPrompt::Placement(request) => TerminalPromptResponse::Placement(
                            prompt_peer_placement(&request, &mut input, &mut output)
                                .map_err(|error| error.to_string()),
                        ),
                    };
                    if prompt_response_tx.blocking_send(response).is_err() {
                        break;
                    }
                }
            })?;
    }
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
                    if interactive {
                        prompt_request_tx
                            .send(TerminalPrompt::Pairing(request))
                            .await
                            .context("terminal prompt worker stopped")?;
                    } else {
                        println!(
                            "\nPairing request\n\nRequest: {}\nDevice: {}\nAddress: {}\nFingerprint: {}\nPairing code: {}\n\nNo interactive terminal; decide with `rflow peers accept {}` or `rflow peers reject {}`.",
                            request.request_id,
                            request.device_name,
                            request.address,
                            request.fingerprint,
                            request.code,
                            request.request_id,
                            request.request_id
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
                Some(AppEvent::PlacementRequested(request)) => {
                    if interactive {
                        prompt_request_tx
                            .send(TerminalPrompt::Placement(request))
                            .await
                            .context("terminal prompt worker stopped")?;
                    } else {
                        println!(
                            "New device {} is unplaced. Run `rflow layout place <screen> --right-of <local-screen>` to place it later.",
                            request.device_name
                        );
                    }
                }
                Some(AppEvent::TopologyChanged(_)) => {}
                Some(AppEvent::Faulted(error)) => {
                    fault = Some(error);
                    break;
                }
                None => bail!("rflow runtime stopped without a terminal event"),
            },
            Some(response) = prompt_response_rx.recv() => match response {
                TerminalPromptResponse::Pairing(request_id, accepted) => {
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
                TerminalPromptResponse::Placement(Ok(Some(decision))) => {
                    match application
                        .apply_layout(
                            decision.revision,
                            LayoutCommand::Place {
                                screen_id: decision.screen_id,
                                anchor_id: decision.anchor_id,
                                position: decision.position,
                                replace: false,
                            },
                        )
                        .await
                    {
                        Ok(topology) => println!("Layout updated to revision {}.", topology.revision),
                        Err(error) => eprintln!("Could not place the new device: {error:#}"),
                    }
                }
                TerminalPromptResponse::Placement(Ok(None)) => {
                    println!("Device left unplaced. Configure it later with `rflow layout place`.");
                }
                TerminalPromptResponse::Placement(Err(error)) => {
                    eprintln!("Could not read placement choice: {error}");
                }
            },
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
                    retry_for: match retry_for {
                        RetryFor::Duration(duration) => Some(duration),
                        RetryFor::Forever => None,
                    },
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
    fn verbosity_is_a_repeatable_global_runtime_option() {
        let before = Cli::try_parse_from(["rflow", "-v", "host"]).unwrap();
        assert_eq!(before.verbose, 1);

        let after = Cli::try_parse_from(["rflow", "client", "desktop.local", "-vv"]).unwrap();
        assert_eq!(after.verbose, 2);
    }

    #[tokio::test]
    async fn offline_layout_read_does_not_require_management_socket() {
        let directory = tempfile::tempdir().unwrap();
        let management_path = directory.path().join("missing.sock");
        let topology_path = directory.path().join("topology.json");

        let topology = request_topology(&management_path, &topology_path)
            .await
            .expect("offline layout should load from local storage");

        assert_eq!(topology, ScreenTopology::default());
    }

    #[tokio::test]
    async fn offline_layout_mutation_persists_without_management_socket() {
        let directory = tempfile::tempdir().unwrap();
        let management_path = directory.path().join("missing.sock");
        let topology_path = directory.path().join("topology.json");

        submit_layout_command(
            &management_path,
            &topology_path,
            0,
            LayoutCommand::Replace {
                layout: ScreenLayout::default(),
            },
        )
        .await
        .unwrap();

        let state = rflow::topology_store::load_state(&topology_path).unwrap();
        assert_eq!(state.layout.revision, 1);
    }

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
        let Command::Host {
            bind, size, device, ..
        } = cli.command
        else {
            panic!("expected host command")
        };
        assert_eq!(bind, "0.0.0.0:24801".parse().unwrap());
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
                assert_eq!(retry_for, RetryFor::Duration(Duration::ZERO));
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
            Command::Client { retry_for, .. } => {
                assert_eq!(retry_for, RetryFor::Duration(Duration::from_secs(120)))
            }
            _ => panic!("expected client command"),
        }
    }

    #[test]
    fn client_accepts_infinite_retry_window() {
        let cli = Cli::try_parse_from([
            "rflow",
            "client",
            "192.168.1.50:24801",
            "--retry-for",
            "forever",
        ])
        .unwrap();
        match cli.command {
            Command::Client { retry_for, .. } => assert_eq!(retry_for, RetryFor::Forever),
            _ => panic!("expected client command"),
        }
    }

    #[test]
    fn peers_forget_requires_an_explicit_device_and_supports_yes() {
        let cli = Cli::try_parse_from(["rflow", "peers", "forget", "macmini", "--yes"]).unwrap();
        let Command::Peers {
            command: Some(PeersCommand::Forget { device, all, yes }),
        } = cli.command
        else {
            panic!("expected peers forget command")
        };
        assert_eq!(device.as_deref(), Some("macmini"));
        assert!(!all);
        assert!(yes);
    }

    #[test]
    fn peers_forget_all_is_explicit_and_mutually_exclusive_with_a_device() {
        assert!(Cli::try_parse_from(["rflow", "peers", "forget", "--all", "--yes"]).is_ok());
        assert!(Cli::try_parse_from(["rflow", "peers", "forget", "--yes"]).is_err());
        assert!(
            Cli::try_parse_from(["rflow", "peers", "forget", "macmini", "--all", "--yes"]).is_err()
        );
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
    fn placement_prompt_marks_occupied_positions_and_rejects_them() {
        let options = vec![
            PlacementOptionSummary {
                position: RelativePosition::LeftOf,
                occupied_by: Vec::new(),
            },
            PlacementOptionSummary {
                position: RelativePosition::RightOf,
                occupied_by: vec!["existing/display".into()],
            },
            PlacementOptionSummary {
                position: RelativePosition::Above,
                occupied_by: Vec::new(),
            },
            PlacementOptionSummary {
                position: RelativePosition::Below,
                occupied_by: Vec::new(),
            },
        ];
        let request = PlacementRequestSummary {
            device_name: "macmini".into(),
            revision: 7,
            anchor_screen: ScreenId("host/main".into()),
            anchor_name: "host/main".into(),
            screens: vec![
                PlacementScreenSummary {
                    screen_id: ScreenId("macmini/main".into()),
                    name: "main".into(),
                    options: options.clone(),
                },
                PlacementScreenSummary {
                    screen_id: ScreenId("macmini/secondary".into()),
                    name: "secondary".into(),
                    options,
                },
            ],
        };
        let mut input = io::Cursor::new(b"1\n2\n1\n");
        let mut output = Vec::new();

        let decision = prompt_peer_placement(&request, &mut input, &mut output)
            .unwrap()
            .unwrap();

        assert_eq!(decision.screen_id, ScreenId("macmini/main".into()));
        assert_eq!(decision.position, RelativePosition::LeftOf);
        let output = String::from_utf8(output).unwrap();
        assert!(output.contains("Right [unavailable: occupied by existing/display]"));
        assert!(output.contains("That position is unavailable."));
    }

    #[test]
    fn retired_connect_command_is_not_exposed() {
        assert!(Cli::try_parse_from(["rflow", "connect", "192.168.1.50:24801"]).is_err());
    }

    #[test]
    fn layout_place_parses_one_relative_direction() {
        let cli = Cli::try_parse_from([
            "rflow",
            "layout",
            "place",
            "macmini",
            "--right-of",
            "linux/DP-1",
            "--replace",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Layout {
                command: Some(LayoutSubcommand::Place { screen, right_of: Some(anchor), replace: true, .. }),
                ..
            } if screen == "macmini" && anchor == "linux/DP-1"
        ));
    }

    #[test]
    fn screen_name_resolution_rejects_multi_display_device_shorthand() {
        let topology = rflow::core::ScreenTopology {
            screens: vec![
                screen_node("device/a", "device", "DP-1"),
                screen_node("device/b", "device", "DP-2"),
            ],
            ..Default::default()
        };
        let error = resolve_screen_id(&topology, "device").unwrap_err();
        assert!(error.to_string().contains("multiple screens"));
        assert!(error.to_string().contains("device/a"));
        assert_eq!(resolve_screen_id(&topology, "DP-1").unwrap().0, "device/a");
    }

    #[test]
    fn peers_summary_reports_partial_multi_display_placement() {
        let mut topology = ScreenTopology {
            screens: vec![
                screen_node("device/a", "device", "DP-1"),
                screen_node("device/b", "device", "DP-2"),
            ],
            ..Default::default()
        };
        topology.links.push(rflow::core::ScreenLink {
            from: ScreenEdge {
                screen_id: ScreenId("device/a".into()),
                edge: Edge::Right,
            },
            to: ScreenEdge {
                screen_id: ScreenId("other".into()),
                edge: Edge::Left,
            },
        });
        assert_eq!(
            peer_layout_summary(&topology, "device"),
            (true, 2, "partially placed")
        );
    }

    fn screen_node(id: &str, device: &str, name: &str) -> rflow::core::ScreenNode {
        rflow::core::ScreenNode {
            screen_id: rflow::core::ScreenId(id.into()),
            device_id: rflow::core::TopologyDeviceId(device.into()),
            device_name: device.into(),
            name: name.into(),
            logical_size: ScreenSize::new(100, 100).unwrap(),
            size_override: None,
            online: true,
            this_device: false,
        }
    }
}
