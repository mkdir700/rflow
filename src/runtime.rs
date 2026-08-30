use std::{
    collections::{HashMap, VecDeque},
    fs,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot, watch};

use crate::{
    core::{
        ButtonState, ControlTarget, DesktopSession, HeldInput, InputEvent, LayoutCommand, Motion,
        ScreenDescriptor, ScreenDirection, ScreenId, ScreenInventory, ScreenSize, ScreenTopology,
        SessionEffect, SessionEvent, TopologyDeviceId,
    },
    identity::IdentityPaths,
    pairing::{
        PAIRING_PROTOCOL_VERSION, PairingCode, PairingMaterial, PairingMessage, PairingRequestId,
        PairingRole, pairing_proof,
    },
    platform::{self, CapturedEvent, InputInjector},
    protocol::{
        MAX_RELIABLE_FRAME, MotionDto, PROTOCOL_VERSION, ReliableEvent, WireScreenDescriptor,
        decode, decode_input, encode, encode_frame, encode_input,
    },
    target::ServerTarget,
    transport,
    trust::{DeviceId, TrustStore, VerifyPeer},
};

const PAIRING_REQUEST_LIFETIME: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub size: Option<ScreenSize>,
    pub devices: Vec<PathBuf>,
    pub direction: Option<ScreenDirection>,
    pub device_name: String,
    pub trust_store: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub target: ServerTarget,
    pub identity_cert: PathBuf,
    pub identity_key: PathBuf,
    pub server_cert: Option<PathBuf>,
    pub size: Option<ScreenSize>,
    pub retry_for: Duration,
    pub device_name: String,
    pub trust_store: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingRequestSummary {
    pub request_id: PairingRequestId,
    pub device_name: String,
    pub address: SocketAddr,
    pub fingerprint: DeviceId,
    pub code: PairingCode,
    pub expires_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerSummary {
    pub device_id: DeviceId,
    pub display_name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementOptionSummary {
    pub position: crate::core::RelativePosition,
    pub occupied_by: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementScreenSummary {
    pub screen_id: ScreenId,
    pub name: String,
    pub options: Vec<PlacementOptionSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlacementRequestSummary {
    pub device_name: String,
    pub revision: u64,
    pub anchor_screen: ScreenId,
    pub anchor_name: String,
    pub screens: Vec<PlacementScreenSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppConfig {
    pub host: Option<HostConfig>,
    pub client: Option<ClientConfig>,
}

#[derive(Debug)]
/// User intent accepted by the single-writer Runtime supervisor.
/// Commands from one handle are processed in FIFO order.
pub enum AppCommand {
    StartHost(HostConfig),
    StartClient(ClientConfig),
    Stop,
    UpdateConfig(AppConfig),
    AcceptPairing(PairingRequestId),
    RejectPairing(PairingRequestId),
    ForgetPeer(DeviceId),
    ReplaceTopology {
        expected_revision: u64,
        topology: ScreenTopology,
    },
    ApplyLayout {
        expected_revision: u64,
        command: LayoutCommand,
        respond_to: oneshot::Sender<std::result::Result<ScreenTopology, String>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Stopped,
    Starting,
    Listening,
    PairingPending,
    Connecting,
    Connected,
    Retrying,
    Stopping,
    ConfigurationRequired,
    Faulted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultKind {
    InvalidConfiguration,
    PermissionDenied,
    PlatformUnavailable,
    Connection,
    Protocol,
    TaskStopped,
    Cleanup,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppFault {
    pub kind: FaultKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
/// Ordered application notifications. High-frequency/control notifications may
/// be coalesced under pressure; `RuntimeHandle::snapshot` is authoritative.
pub enum AppEvent {
    StatusChanged(RuntimeStatus),
    PeerChanged(Option<SocketAddr>),
    PeerIdentified(PeerSummary),
    ControlChanged(ControlTarget),
    ConfigChanged(Box<AppConfig>),
    PairingRequested(Box<PairingRequestSummary>),
    PairingCodeReady(Box<PairingRequestSummary>),
    PairingCleared(PairingRequestId),
    PairingExpired(PairingRequestId),
    PeerTrusted(PeerSummary),
    PlacementRequested(Box<PlacementRequestSummary>),
    TopologyChanged(ScreenTopology),
    Faulted(AppFault),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub peer: Option<SocketAddr>,
    pub peer_device: Option<PeerSummary>,
    pub control: Option<ControlTarget>,
    pub config: AppConfig,
    pub fault: Option<AppFault>,
    pub pairing: Option<PairingRequestSummary>,
    pub topology: ScreenTopology,
}

impl Default for RuntimeSnapshot {
    fn default() -> Self {
        Self {
            status: RuntimeStatus::Stopped,
            peer: None,
            peer_device: None,
            control: None,
            config: AppConfig::default(),
            fault: None,
            pairing: None,
            topology: ScreenTopology::default(),
        }
    }
}

#[derive(Clone)]
struct AppEventBus {
    sender: mpsc::Sender<AppEvent>,
    snapshot: Arc<std::sync::RwLock<RuntimeSnapshot>>,
}

impl AppEventBus {
    fn snapshot_topology(&self) -> ScreenTopology {
        self.snapshot
            .read()
            .expect("runtime snapshot poisoned")
            .topology
            .clone()
    }

    async fn send(&self, event: AppEvent) {
        self.apply(&event);
        let _ = self.sender.send(event).await;
    }

    fn try_send(&self, event: AppEvent) {
        self.apply(&event);
        let _ = self.sender.try_send(event);
    }

    fn apply(&self, event: &AppEvent) {
        let mut snapshot = self.snapshot.write().expect("runtime snapshot poisoned");
        match event {
            AppEvent::StatusChanged(status) => {
                snapshot.status = *status;
                if *status == RuntimeStatus::Starting {
                    snapshot.peer = None;
                    snapshot.peer_device = None;
                    snapshot.control = None;
                    snapshot.fault = None;
                } else if matches!(
                    status,
                    RuntimeStatus::Stopped
                        | RuntimeStatus::ConfigurationRequired
                        | RuntimeStatus::Faulted
                ) {
                    snapshot.control = None;
                }
            }
            AppEvent::PeerChanged(peer) => {
                snapshot.peer = *peer;
                if peer.is_none() {
                    snapshot.peer_device = None;
                }
            }
            AppEvent::PeerIdentified(peer) => snapshot.peer_device = Some(peer.clone()),
            AppEvent::ControlChanged(control) => snapshot.control = Some(*control),
            AppEvent::ConfigChanged(config) => snapshot.config = (**config).clone(),
            AppEvent::PairingRequested(request) => snapshot.pairing = Some((**request).clone()),
            AppEvent::PairingCodeReady(request) => snapshot.pairing = Some((**request).clone()),
            AppEvent::PairingCleared(request_id) | AppEvent::PairingExpired(request_id) => {
                if snapshot
                    .pairing
                    .as_ref()
                    .is_some_and(|request| request.request_id == *request_id)
                {
                    snapshot.pairing = None;
                }
            }
            AppEvent::PeerTrusted(_) | AppEvent::PlacementRequested(_) => {}
            AppEvent::TopologyChanged(topology) => snapshot.topology = topology.clone(),
            AppEvent::Faulted(fault) => snapshot.fault = Some(fault.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiagnosticEvent {
    ConnectionAttempt { attempt: u32, remote: SocketAddr },
    MotionDropped { sequence: u64 },
    SessionEnded { message: String },
}

pub trait DiagnosticSink: Send + Sync + 'static {
    fn emit(&self, event: DiagnosticEvent);
}

#[derive(Default)]
pub struct NoopDiagnostics;

impl DiagnosticSink for NoopDiagnostics {
    fn emit(&self, _event: DiagnosticEvent) {}
}

#[derive(Default)]
pub struct TracingDiagnostics;

impl DiagnosticSink for TracingDiagnostics {
    fn emit(&self, event: DiagnosticEvent) {
        tracing::debug!(?event, "runtime diagnostic");
    }
}

struct BufferedDiagnostics {
    sender: std::sync::mpsc::SyncSender<DiagnosticEvent>,
}

impl DiagnosticSink for BufferedDiagnostics {
    fn emit(&self, event: DiagnosticEvent) {
        // Diagnostics are explicitly lossy under pressure and never block the
        // input path. Application state uses a separate channel.
        let _ = self.sender.try_send(event);
    }
}

/// Process-local Application seam used by CLI and the future GPUI adapter.
pub struct RuntimeHandle {
    commands: mpsc::Sender<AppCommand>,
    events: mpsc::Receiver<AppEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
    snapshot: Arc<std::sync::RwLock<RuntimeSnapshot>>,
    completion: Option<tokio::sync::oneshot::Receiver<()>>,
}

/// Cloneable Application capability for presentation and local-management adapters.
/// It exposes commands and the authoritative projection, but not Runtime internals.
#[derive(Clone)]
pub struct ApplicationHandle {
    commands: mpsc::Sender<AppCommand>,
    snapshot: Arc<std::sync::RwLock<RuntimeSnapshot>>,
}

impl ApplicationHandle {
    pub async fn send(&self, command: AppCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .context("rflow runtime stopped")
    }

    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot
            .read()
            .expect("runtime snapshot poisoned")
            .clone()
    }

    pub async fn apply_layout(
        &self,
        expected_revision: u64,
        command: LayoutCommand,
    ) -> Result<ScreenTopology> {
        let (respond_to, response) = oneshot::channel();
        self.send(AppCommand::ApplyLayout {
            expected_revision,
            command,
            respond_to,
        })
        .await?;
        response
            .await
            .context("rflow runtime dropped the layout response")?
            .map_err(anyhow::Error::msg)
    }
}

#[cfg(all(test, unix))]
pub(crate) fn application_test_channel(
    snapshot: RuntimeSnapshot,
) -> (ApplicationHandle, mpsc::Receiver<AppCommand>) {
    let (commands, receiver) = mpsc::channel(4);
    (
        ApplicationHandle {
            commands,
            snapshot: Arc::new(std::sync::RwLock::new(snapshot)),
        },
        receiver,
    )
}

impl RuntimeHandle {
    /// Starts an isolated Tokio Runtime thread and a bounded diagnostics worker.
    pub fn spawn(diagnostics: Arc<dyn DiagnosticSink>) -> Result<Self> {
        let (commands, command_rx) = mpsc::channel(16);
        let (event_tx, events) = mpsc::channel(128);
        let snapshot = Arc::new(std::sync::RwLock::new(RuntimeSnapshot::default()));
        let event_bus = AppEventBus {
            sender: event_tx,
            snapshot: snapshot.clone(),
        };
        let (completion_tx, completion) = tokio::sync::oneshot::channel();
        let thread = std::thread::Builder::new()
            .name("rflow-runtime".to_owned())
            .spawn(move || {
                let (diagnostic_tx, diagnostic_rx) = std::sync::mpsc::sync_channel(256);
                let diagnostic_thread = std::thread::Builder::new()
                    .name("rflow-diagnostics".to_owned())
                    .spawn(move || {
                        while let Ok(event) = diagnostic_rx.recv() {
                            diagnostics.emit(event);
                        }
                    })
                    .expect("spawn diagnostics worker");
                let buffered: Arc<dyn DiagnosticSink> = Arc::new(BufferedDiagnostics {
                    sender: diagnostic_tx,
                });
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("build rflow Tokio runtime");
                runtime.block_on(supervisor(command_rx, event_bus, buffered));
                drop(runtime);
                let _ = diagnostic_thread.join();
                let _ = completion_tx.send(());
            })
            .context("spawn rflow runtime thread")?;
        Ok(Self {
            commands,
            events,
            thread: Some(thread),
            snapshot,
            completion: Some(completion),
        })
    }

    /// Enqueues a command with bounded backpressure and FIFO ordering.
    pub async fn send(&self, command: AppCommand) -> Result<()> {
        self.commands
            .send(command)
            .await
            .context("rflow runtime stopped")
    }

    pub fn application_handle(&self) -> ApplicationHandle {
        ApplicationHandle {
            commands: self.commands.clone(),
            snapshot: self.snapshot.clone(),
        }
    }

    /// Waits for the next application notification. Consumers must read
    /// `snapshot` after notification to obtain complete current state.
    pub async fn next_event(&mut self) -> Option<AppEvent> {
        self.events.recv().await
    }

    /// Returns the latest complete state projection without waiting for events.
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.snapshot
            .read()
            .expect("runtime snapshot poisoned")
            .clone()
    }

    /// Idempotently stops the active session, waits for input cleanup, and
    /// terminates the supervisor thread. This future does not require a Tokio
    /// executor and can be polled by GPUI.
    pub async fn shutdown(mut self) -> Result<()> {
        let _ = self.commands.send(AppCommand::Stop).await;
        while let Some(event) = self.events.recv().await {
            if matches!(
                event,
                AppEvent::StatusChanged(
                    RuntimeStatus::Stopped
                        | RuntimeStatus::ConfigurationRequired
                        | RuntimeStatus::Faulted
                )
            ) {
                break;
            }
        }
        // The first Stop may have ended an active session. A second idempotent
        // Stop terminates the now-idle supervisor before joining its thread.
        let _ = self.commands.send(AppCommand::Stop).await;
        if let Some(completion) = self.completion.take() {
            let _ = completion.await;
        }
        if let Some(thread) = self.thread.take() {
            thread
                .join()
                .map_err(|_| anyhow::anyhow!("rflow runtime thread panicked"))?;
        }
        Ok(())
    }
}

impl Drop for RuntimeHandle {
    fn drop(&mut self) {
        let _ = self.commands.try_send(AppCommand::Stop);
    }
}

async fn supervisor(
    mut commands: mpsc::Receiver<AppCommand>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
) {
    while let Some(command) = commands.recv().await {
        let session = match command {
            AppCommand::StartHost(config) => SessionKind::Host(config),
            AppCommand::StartClient(config) => SessionKind::Client(config),
            AppCommand::Stop => {
                publish_status(&events, RuntimeStatus::Stopped).await;
                break;
            }
            AppCommand::UpdateConfig(config) => {
                events.send(AppEvent::ConfigChanged(Box::new(config))).await;
                continue;
            }
            AppCommand::AcceptPairing(_) | AppCommand::RejectPairing(_) => continue,
            AppCommand::ForgetPeer(device_id) => {
                mark_peer_offline(device_id, &events).await;
                continue;
            }
            AppCommand::ReplaceTopology {
                expected_revision,
                topology,
            } => {
                match crate::topology_store::replace(
                    &crate::topology_store::default_path().expect("resolve topology path"),
                    expected_revision,
                    topology,
                ) {
                    Ok(topology) => events.send(AppEvent::TopologyChanged(topology)).await,
                    Err(error) => events.send(AppEvent::Faulted(classify_fault(&error))).await,
                }
                continue;
            }
            AppCommand::ApplyLayout {
                expected_revision,
                command,
                respond_to,
            } => {
                let result = apply_layout_command(expected_revision, command);
                if let Ok(topology) = &result {
                    events
                        .send(AppEvent::TopologyChanged(topology.clone()))
                        .await;
                }
                let _ = respond_to.send(result.map_err(|error| format!("{error:#}")));
                continue;
            }
        };
        publish_status(&events, RuntimeStatus::Starting).await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (topology_tx, topology_rx) = watch::channel(events.snapshot_topology());
        let (pairing_tx, pairing_rx) = mpsc::channel(8);
        let (forget_tx, forget_rx) = mpsc::channel(8);
        let session_events = events.clone();
        let session_diagnostics = diagnostics.clone();
        let mut task = tokio::spawn(async move {
            match session {
                SessionKind::Host(config) => {
                    run_host(
                        config,
                        stop_rx,
                        pairing_rx,
                        topology_rx,
                        forget_rx,
                        session_events,
                        session_diagnostics,
                    )
                    .await
                }
                SessionKind::Client(config) => {
                    run_client(config, stop_rx, session_events, session_diagnostics).await
                }
            }
        });

        loop {
            tokio::select! {
                result = &mut task => {
                    finish_session(result, &events, &diagnostics).await;
                    break;
                }
                command = commands.recv() => match command {
                    Some(AppCommand::Stop) | None => {
                        publish_status(&events, RuntimeStatus::Stopping).await;
                        let _ = stop_tx.send(true);
                        finish_session(task.await, &events, &diagnostics).await;
                        break;
                    }
                    Some(AppCommand::UpdateConfig(config)) => {
                        events
                            .send(AppEvent::ConfigChanged(Box::new(config)))
                            .await;
                    }
                    Some(AppCommand::AcceptPairing(request_id)) => {
                        let _ = pairing_tx
                            .send(PairingDecision {
                                request_id,
                                accepted: true,
                            })
                            .await;
                    }
                    Some(AppCommand::ReplaceTopology { expected_revision, topology }) => {
                        match crate::topology_store::replace(
                            &crate::topology_store::default_path().expect("resolve topology path"),
                            expected_revision,
                            topology,
                        ) {
                            Ok(topology) => {
                                let _ = topology_tx.send(topology.clone());
                                events.send(AppEvent::TopologyChanged(topology)).await;
                            }
                            Err(error) => events.send(AppEvent::Faulted(classify_fault(&error))).await,
                        }
                    }
                    Some(AppCommand::ApplyLayout { expected_revision, command, respond_to }) => {
                        let result = apply_layout_command(expected_revision, command);
                        if let Ok(topology) = &result {
                            let _ = topology_tx.send(topology.clone());
                            events.send(AppEvent::TopologyChanged(topology.clone())).await;
                        }
                        let _ = respond_to.send(result.map_err(|error| format!("{error:#}")));
                    }
                    Some(AppCommand::RejectPairing(request_id)) => {
                        let _ = pairing_tx
                            .send(PairingDecision {
                                request_id,
                                accepted: false,
                            })
                            .await;
                    }
                    Some(AppCommand::ForgetPeer(device_id)) => {
                        let _ = forget_tx.send(device_id).await;
                    }
                    Some(AppCommand::StartHost(_)) | Some(AppCommand::StartClient(_)) => {
                        let fault = AppFault {
                            kind: FaultKind::InvalidConfiguration,
                            message: "a session is already running".to_owned(),
                        };
                        events.send(AppEvent::Faulted(fault)).await;
                    }
                }
            }
        }
    }
}

fn apply_layout_command(expected_revision: u64, command: LayoutCommand) -> Result<ScreenTopology> {
    let path = crate::topology_store::default_path()?;
    let inventory = crate::topology_store::load_state(&path)?.inventory;
    crate::topology_store::apply(&path, expected_revision, &inventory, command)
}

enum SessionKind {
    Host(HostConfig),
    Client(ClientConfig),
}

struct PairingDecision {
    request_id: PairingRequestId,
    accepted: bool,
}

async fn finish_session(
    result: std::result::Result<Result<()>, tokio::task::JoinError>,
    events: &AppEventBus,
    diagnostics: &Arc<dyn DiagnosticSink>,
) {
    events.try_send(AppEvent::PeerChanged(None));
    match result {
        Ok(Ok(())) => publish_status(events, RuntimeStatus::Stopped).await,
        Ok(Err(error)) => {
            diagnostics.emit(DiagnosticEvent::SessionEnded {
                message: format!("{error:#}"),
            });
            let fault = classify_fault(&error);
            let status = if matches!(
                fault.kind,
                FaultKind::PermissionDenied | FaultKind::PlatformUnavailable
            ) {
                RuntimeStatus::ConfigurationRequired
            } else {
                RuntimeStatus::Faulted
            };
            publish_status(events, status).await;
            events.send(AppEvent::Faulted(fault)).await;
        }
        Err(error) => {
            let fault = AppFault {
                kind: FaultKind::TaskStopped,
                message: format!("runtime task stopped: {error}"),
            };
            publish_status(events, RuntimeStatus::Faulted).await;
            events.send(AppEvent::Faulted(fault)).await;
        }
    }
}

fn classify_fault(error: &anyhow::Error) -> AppFault {
    let message = format!("{error:#}");
    let lower = message.to_ascii_lowercase();
    let kind = if lower.contains("permission") || lower.contains("accessibility") {
        FaultKind::PermissionDenied
    } else if lower.contains("identity certificate")
        || lower.contains("private key")
        || lower.contains("invalid")
        || lower.contains("configuration")
    {
        FaultKind::InvalidConfiguration
    } else if lower.contains("protocol") || lower.contains("handshake") {
        FaultKind::Protocol
    } else if lower.contains("connect") || lower.contains("connection") {
        FaultKind::Connection
    } else if lower.contains("capture") || lower.contains("input device") {
        FaultKind::PlatformUnavailable
    } else {
        FaultKind::Internal
    };
    AppFault { kind, message }
}

async fn publish_status(events: &AppEventBus, status: RuntimeStatus) {
    events.send(AppEvent::StatusChanged(status)).await;
}

async fn run_host(
    mut config: HostConfig,
    mut stop: watch::Receiver<bool>,
    mut pairing_decisions: mpsc::Receiver<PairingDecision>,
    mut topology_updates: watch::Receiver<ScreenTopology>,
    mut forgotten_peers: mpsc::Receiver<DeviceId>,
    events: AppEventBus,
    _diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let mut detected_screens = platform::screens()?;
    let size = config.size.unwrap_or(
        detected_screens
            .iter()
            .find(|screen| screen.primary)
            .or_else(|| detected_screens.first())
            .context("platform reported no active screens")?
            .logical_size,
    );
    config.size = Some(size);
    config.devices = platform::resolve_capture_devices(&config.devices)?;
    platform::validate_capture(&config.devices)?;
    let identity = IdentityPaths {
        certificate: config.cert.clone(),
        private_key: config.key.clone(),
    };
    let local_device_id = DeviceId::from_certificate(
        &fs::read(&config.cert).context("read host identity certificate")?,
    );
    let topology_path = crate::topology_store::default_path()?;
    let state = crate::topology_store::load_state(&topology_path)?;
    let inventory = reconcile_discovered_inventory(
        state.inventory,
        &config.device_name,
        local_device_id,
        &detected_screens,
    );
    crate::topology_store::save_inventory(&topology_path, &inventory)?;
    let mut layout = state.layout;
    let mut inventory = inventory;
    let mut topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
    events
        .send(AppEvent::TopologyChanged(topology.clone()))
        .await;
    let endpoint = transport::pairing_server_endpoint(config.bind, &identity)?;
    let mut trust = TrustStore::load(&config.trust_store)?;
    let mut pairing_rate_limiter = PairingRateLimiter::default();
    tracing::info!(local = %endpoint.local_addr()?, "host listening");
    publish_status(&events, RuntimeStatus::Listening).await;
    let local_screen = topology
        .screens
        .iter()
        .find(|screen| screen.this_device && screen.online)
        .context("no online local screen is available")?
        .screen_id
        .clone();
    let (cursor_x, cursor_y) = platform::cursor_position().unwrap_or((0, 0));
    let mut session = DesktopSession::host_topology(&topology, local_screen, cursor_x, cursor_y)
        .map_err(anyhow::Error::msg)?;
    let mut peers: HashMap<TopologyDeviceId, HostPeer> = HashMap::new();
    let (disconnected_tx, mut disconnected_rx) = mpsc::channel(64);
    let (inventory_tx, mut inventory_rx) = mpsc::channel(64);
    let mut capture = None;
    let mut injector = None;
    let mut held_sources = HashMap::new();
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            accepted = transport::accept_one(&endpoint) => {
                let connection = accepted?;
                let remote = connection.remote_address();
                let Some(authorized_peer) = authorize_host_peer(
                    &connection,
                    &config,
                    &mut trust,
                    &mut pairing_rate_limiter,
                    &mut pairing_decisions,
                    &mut stop,
                    &events,
                ).await? else {
                    connection.close(0_u32.into(), b"pairing rejected");
                    continue;
                };
                let newly_trusted = authorized_peer.newly_trusted;
                let (host_peer, remote_screens, mut metadata) = prepare_host_peer(
                    connection,
                    authorized_peer.summary,
                    &detected_screens,
                    &mut stop,
                ).await?;
                merge_remote_inventory(&mut inventory, &host_peer.summary, &remote_screens);
                crate::topology_store::save_inventory(&topology_path, &inventory)?;
                if !layout.links.is_empty() && config.direction.is_some() {
                    bail!("--direction cannot be used after a persistent layout has been configured");
                }
                if layout.links.is_empty()
                    && let Some(direction) = config.direction.take()
                {
                    tracing::warn!(%direction, "migrating legacy --direction into the persistent layout");
                    let migrated = migrate_legacy_direction(&layout, &inventory, direction)?;
                    topology = crate::topology_store::apply(
                        &topology_path,
                        layout.revision,
                        &inventory,
                        LayoutCommand::Replace { layout: migrated },
                    )?;
                    layout = crate::topology_store::load_state(&topology_path)?.layout;
                } else {
                    topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
                }
                let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                execute_host_effects_multi(
                    effects,
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: None, held: &held_sources },
                ).await?;
                let device_id = TopologyDeviceId(host_peer.summary.device_id.to_string());
                if let Some(previous) = peers.insert(device_id.clone(), host_peer) {
                    previous.connection.close(0_u32.into(), b"replaced by a newer connection");
                }
                let connection = peers[&device_id].connection.clone();
                let disconnected = disconnected_tx.clone();
                let inventory_updates = inventory_tx.clone();
                let inventory_device_id = device_id.clone();
                let disconnected_device_id = device_id.clone();
                tokio::spawn(async move {
                    let _ = connection.closed().await;
                    let _ = disconnected.send((disconnected_device_id, remote)).await;
                });
                tokio::spawn(async move {
                    loop {
                        let screens = match read_reliable(&mut metadata).await {
                            Ok(ReliableEvent::ScreenInventory { screens }) => {
                                match validate_wire_screens(screens) {
                                    Ok(screens) => screens,
                                    Err(_) => break,
                                }
                            }
                            _ => break,
                        };
                        if inventory_updates
                            .send((inventory_device_id.clone(), remote, screens))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                });
                if capture.is_none() {
                    injector = Some(InputInjector::new(&config.devices)?);
                    capture = Some(platform::capture(config.devices.clone(), true)?);
                    events.try_send(AppEvent::ControlChanged(ControlTarget::Local));
                }
                events.send(AppEvent::PeerChanged(Some(remote))).await;
                events.send(AppEvent::TopologyChanged(topology.clone())).await;
                if let Some(request) = placement_request(
                    newly_trusted,
                    host_peer_summary(&peers, &device_id)?,
                    &inventory,
                    &topology,
                )? {
                    events.send(AppEvent::PlacementRequested(Box::new(request))).await;
                }
                publish_status(&events, RuntimeStatus::Connected).await;
                tracing::info!(%remote, peers = peers.len(), "client connected");
            }
            captured = next_host_capture(&mut capture) => {
                let (source, event, released) = match captured? {
                    CapturedEvent::Input { source, event, .. } => {
                        let held = held_input(event);
                        if matches!(input_state(event), Some(ButtonState::Pressed))
                            && let Some(held) = held
                        {
                            held_sources.insert(held, source);
                        }
                        (source, SessionEvent::PhysicalInput(event),
                         matches!(input_state(event), Some(ButtonState::Released)).then_some(held).flatten())
                    }
                    CapturedEvent::Motion { source, motion } => {
                        (source, SessionEvent::PhysicalMotion(motion), None)
                    }
                };
                execute_host_effects_multi(
                    session.handle(event),
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: Some(source), held: &held_sources },
                ).await?;
                if let Some(held) = released { held_sources.remove(&held); }
            }
            Some((device_id, remote)) = disconnected_rx.recv() => {
                let matches_current = peers
                    .get(&device_id)
                    .is_some_and(|peer| peer.connection.remote_address() == remote);
                if !matches_current {
                    continue;
                }
                peers.remove(&device_id);
                for screen in inventory.screens.iter_mut().filter(|screen| screen.device_id == device_id) {
                    screen.online = false;
                }
                crate::topology_store::save_inventory(&topology_path, &inventory)?;
                layout = crate::topology_store::load_state(&topology_path)?.layout;
                topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
                let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                execute_host_effects_multi(
                    effects,
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: None, held: &held_sources },
                ).await?;
                events.send(AppEvent::TopologyChanged(topology.clone())).await;
                events.send(AppEvent::PeerChanged(peers.values().next().map(|peer| peer.connection.remote_address()))).await;
                publish_status(&events, if peers.is_empty() { RuntimeStatus::Listening } else { RuntimeStatus::Connected }).await;
            }
            Some((device_id, remote, screens)) = inventory_rx.recv() => {
                let Some(peer) = peers.get(&device_id) else { continue };
                if peer.connection.remote_address() != remote {
                    continue;
                }
                merge_remote_inventory(&mut inventory, &peer.summary, &screens);
                crate::topology_store::save_inventory(&topology_path, &inventory)?;
                layout = crate::topology_store::load_state(&topology_path)?.layout;
                topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
                let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                execute_host_effects_multi(
                    effects,
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: None, held: &held_sources },
                ).await?;
                events.send(AppEvent::TopologyChanged(topology.clone())).await;
            }
            Some(device_id) = forgotten_peers.recv() => {
                let device_id = TopologyDeviceId(device_id.to_string());
                if let Some(peer) = peers.remove(&device_id) {
                    peer.connection.close(0_u32.into(), b"peer trust was removed");
                }
                for screen in inventory.screens.iter_mut().filter(|screen| screen.device_id == device_id) {
                    screen.online = false;
                }
                crate::topology_store::save_inventory(&topology_path, &inventory)?;
                layout = crate::topology_store::load_state(&topology_path)?.layout;
                topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
                let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                execute_host_effects_multi(
                    effects,
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: None, held: &held_sources },
                ).await?;
                events.send(AppEvent::TopologyChanged(topology.clone())).await;
            }
            _ = heartbeat.tick() => {
                if let Ok(refreshed) = platform::screens()
                    && !refreshed.is_empty()
                    && wire_screens(&refreshed) != wire_screens(&detected_screens)
                {
                    detected_screens = refreshed;
                    merge_local_inventory(
                        &mut inventory,
                        &config.device_name,
                        local_device_id,
                        &detected_screens,
                    );
                    crate::topology_store::save_inventory(&topology_path, &inventory)?;
                    layout = crate::topology_store::load_state(&topology_path)?.layout;
                    topology = layout.resolve(&inventory).map_err(anyhow::Error::msg)?;
                    let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                    execute_host_effects_multi(
                        effects,
                        &mut peers,
                        &topology,
                        injector.as_mut(),
                        &events,
                        LocalInputSources { current: None, held: &held_sources },
                    ).await?;
                    let screens = wire_screens(&detected_screens);
                    for peer in peers.values_mut() {
                        write_reliable(
                            &mut peer.reliable,
                            &ReliableEvent::ScreenInventory { screens: screens.clone() },
                        ).await?;
                    }
                    events.send(AppEvent::TopologyChanged(topology.clone())).await;
                }
                for peer in peers.values_mut() {
                    peer.heartbeat_sequence = peer.heartbeat_sequence.wrapping_add(1);
                    write_reliable(
                        &mut peer.reliable,
                        &ReliableEvent::Heartbeat { sequence: peer.heartbeat_sequence },
                    ).await?;
                }
            }
            changed = topology_updates.changed() => {
                changed.context("topology update channel closed")?;
                topology = topology_updates.borrow_and_update().clone();
                layout = crate::topology_store::load_state(&topology_path)?.layout;
                let effects = session.replace_topology(&topology).map_err(anyhow::Error::msg)?;
                execute_host_effects_multi(
                    effects,
                    &mut peers,
                    &topology,
                    injector.as_mut(),
                    &events,
                    LocalInputSources { current: None, held: &held_sources },
                ).await?;
            }
            _ = wait_for_stop(&mut stop) => break,
        }
    }

    if let Some(injector) = injector.as_mut() {
        execute_host_effects_multi(
            session.handle(SessionEvent::StopRequested),
            &mut peers,
            &topology,
            Some(injector),
            &events,
            LocalInputSources {
                current: None,
                held: &held_sources,
            },
        )
        .await?;
    }
    for peer in peers.into_values() {
        peer.connection.close(0_u32.into(), b"host stopped");
    }
    for screen in inventory
        .screens
        .iter_mut()
        .filter(|screen| !screen.this_device)
    {
        screen.online = false;
    }
    crate::topology_store::save_inventory(&topology_path, &inventory)?;
    events
        .send(AppEvent::TopologyChanged(
            layout.resolve(&inventory).map_err(anyhow::Error::msg)?,
        ))
        .await;
    Ok(())
}

async fn mark_peer_offline(device_id: DeviceId, events: &AppEventBus) {
    let result: Result<ScreenTopology> = (|| {
        let path = crate::topology_store::default_path()?;
        let state = crate::topology_store::load_state(&path)?;
        let mut inventory = state.inventory;
        let device_id = TopologyDeviceId(device_id.to_string());
        for screen in inventory
            .screens
            .iter_mut()
            .filter(|screen| screen.device_id == device_id)
        {
            screen.online = false;
        }
        crate::topology_store::save_inventory(&path, &inventory)?;
        state.layout.resolve(&inventory).map_err(anyhow::Error::msg)
    })();
    match result {
        Ok(topology) => events.send(AppEvent::TopologyChanged(topology)).await,
        Err(error) => events.send(AppEvent::Faulted(classify_fault(&error))).await,
    }
}

struct HostPeer {
    summary: PeerSummary,
    connection: Connection,
    reliable: SendStream,
    reliable_sequence: u64,
    heartbeat_sequence: u64,
}

fn host_peer_summary<'a>(
    peers: &'a HashMap<TopologyDeviceId, HostPeer>,
    device_id: &TopologyDeviceId,
) -> Result<&'a PeerSummary> {
    peers
        .get(device_id)
        .map(|peer| &peer.summary)
        .context("newly connected peer disappeared before placement prompt")
}

async fn prepare_host_peer(
    connection: Connection,
    summary: PeerSummary,
    local_screens: &[platform::DetectedScreen],
    stop: &mut watch::Receiver<bool>,
) -> Result<(HostPeer, Vec<WireScreenDescriptor>, RecvStream)> {
    let mut metadata = tokio::select! {
        stream = connection.accept_uni() => stream.context("accept client metadata stream")?,
        _ = wait_for_stop(stop) => bail!("host stopped during peer setup"),
    };
    let screens = match read_reliable(&mut metadata).await? {
        ReliableEvent::ClientHello {
            version: PROTOCOL_VERSION,
            screens,
        } => validate_wire_screens(screens)?,
        ReliableEvent::ClientHello { version, .. } => {
            bail!("unsupported client protocol version {version}")
        }
        _ => bail!("first client message must be ClientHello"),
    };
    let mut reliable = connection
        .open_uni()
        .await
        .context("open host input stream")?;
    write_reliable(
        &mut reliable,
        &ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
            screens: wire_screens(local_screens),
        },
    )
    .await?;
    Ok((
        HostPeer {
            summary,
            connection,
            reliable,
            reliable_sequence: 0,
            heartbeat_sequence: 0,
        },
        screens,
        metadata,
    ))
}

async fn next_host_capture(capture: &mut Option<platform::InputCapture>) -> Result<CapturedEvent> {
    match capture {
        Some(capture) => capture.next().await,
        None => std::future::pending().await,
    }
}

async fn execute_host_effects_multi(
    effects: Vec<SessionEffect>,
    peers: &mut HashMap<TopologyDeviceId, HostPeer>,
    topology: &ScreenTopology,
    mut local: Option<&mut InputInjector>,
    events: &AppEventBus,
    sources: LocalInputSources<'_>,
) -> Result<()> {
    for effect in effects {
        match effect {
            SessionEffect::InjectLocal(event) => {
                let local = local
                    .as_deref_mut()
                    .context("local input injector is unavailable")?;
                let source = local_event_source(event, sources.current, sources.held);
                local.emit(source, event)?;
            }
            SessionEffect::InjectLocalMotion { dx, dy } => {
                local
                    .as_deref_mut()
                    .context("local input injector is unavailable")?
                    .emit_motion(sources.current.unwrap_or(0), dx, dy)?;
            }
            SessionEffect::SetLocalCursor { x, y } => {
                local
                    .as_deref_mut()
                    .context("local input injector is unavailable")?
                    .set_cursor_position(x, y)?;
            }
            SessionEffect::SendScreen { screen_id, event } => {
                let peer = peer_for_screen(peers, topology, &screen_id)?;
                peer.reliable_sequence = peer.reliable_sequence.wrapping_add(1);
                write_reliable(
                    &mut peer.reliable,
                    &encode_input(peer.reliable_sequence, event)?,
                )
                .await?;
            }
            SessionEffect::SendScreenMotion { screen_id, motion } => {
                let peer = peer_for_screen(peers, topology, &screen_id)?;
                peer.connection
                    .send_datagram(Bytes::from(encode(&MotionDto::from(motion))?))
                    .context("send remote pointer motion")?;
            }
            SessionEffect::EnterScreen { screen_id, x, y } => {
                let peer = peer_for_screen(peers, topology, &screen_id)?;
                write_reliable(&mut peer.reliable, &ReliableEvent::EnterScreen { x, y }).await?;
            }
            SessionEffect::ReleaseScreen { screen_id } => {
                if let Ok(peer) = peer_for_screen(peers, topology, &screen_id) {
                    write_reliable(&mut peer.reliable, &ReliableEvent::ReleaseAll).await?;
                }
            }
            SessionEffect::SendRemote(event) => {
                let peer = only_peer(peers)?;
                peer.reliable_sequence = peer.reliable_sequence.wrapping_add(1);
                write_reliable(
                    &mut peer.reliable,
                    &encode_input(peer.reliable_sequence, event)?,
                )
                .await?;
            }
            SessionEffect::SendRemoteMotion(motion) => {
                only_peer(peers)?
                    .connection
                    .send_datagram(Bytes::from(encode(&MotionDto::from(motion))?))
                    .context("send remote pointer motion")?;
            }
            SessionEffect::EnterRemote { x, y } => {
                write_reliable(
                    &mut only_peer(peers)?.reliable,
                    &ReliableEvent::EnterScreen { x, y },
                )
                .await?;
            }
            SessionEffect::ReleaseRemote => {
                if peers.len() == 1 {
                    write_reliable(&mut only_peer(peers)?.reliable, &ReliableEvent::ReleaseAll)
                        .await?;
                }
            }
            SessionEffect::ControlChanged(control) => {
                events.try_send(AppEvent::ControlChanged(control));
            }
        }
    }
    Ok(())
}

fn peer_for_screen<'a>(
    peers: &'a mut HashMap<TopologyDeviceId, HostPeer>,
    topology: &ScreenTopology,
    screen_id: &ScreenId,
) -> Result<&'a mut HostPeer> {
    let device_id = topology
        .screens
        .iter()
        .find(|screen| &screen.screen_id == screen_id)
        .with_context(|| format!("screen {} is not in the active topology", screen_id.0))?
        .device_id
        .clone();
    peers
        .get_mut(&device_id)
        .with_context(|| format!("device {} is not connected", device_id.0))
}

fn only_peer(peers: &mut HashMap<TopologyDeviceId, HostPeer>) -> Result<&mut HostPeer> {
    if peers.len() != 1 {
        bail!("legacy remote effect requires exactly one connected peer");
    }
    Ok(peers.values_mut().next().expect("peer count checked"))
}

async fn authorize_host_peer(
    connection: &Connection,
    config: &HostConfig,
    trust: &mut TrustStore,
    rate_limiter: &mut PairingRateLimiter,
    decisions: &mut mpsc::Receiver<PairingDecision>,
    stop: &mut watch::Receiver<bool>,
    events: &AppEventBus,
) -> Result<Option<AuthorizedPeer>> {
    let remote = connection.remote_address();
    let peer_certificate = transport::peer_certificate(connection)?;
    let local_certificate = fs::read(&config.cert).context("read host identity certificate")?;
    let (mut send, mut receive) = tokio::select! {
        stream = connection.accept_bi() => stream.context("accept pairing stream")?,
        _ = wait_for_stop(stop) => return Ok(None),
    };
    let client_message: PairingMessage = read_typed_frame(&mut receive).await?;
    let (client_name, client_material) =
        validate_pairing_hello(client_message, PairingRole::Client, peer_certificate)?;
    events
        .send(AppEvent::PeerIdentified(PeerSummary {
            device_id: DeviceId::from_certificate(&client_material.certificate),
            display_name: client_name.clone(),
        }))
        .await;
    let server_material = PairingMaterial::generate(PairingRole::Server, local_certificate)?;
    write_typed_frame(
        &mut send,
        &server_material.hello(config.device_name.clone()),
    )
    .await?;
    let proof = pairing_proof(&server_material, &client_material)?;

    if matches!(
        trust.verify_certificate(&client_material.certificate),
        VerifyPeer::Trusted(_)
    ) {
        write_typed_frame(&mut send, &PairingMessage::Accepted).await?;
        expect_pairing_acknowledgement(&mut receive).await?;
        send.finish().context("finish pairing stream")?;
        return Ok(Some(AuthorizedPeer {
            summary: PeerSummary {
                device_id: DeviceId::from_certificate(&client_material.certificate),
                display_name: client_name.clone(),
            },
            newly_trusted: false,
        }));
    }

    if !rate_limiter.allow(remote.ip(), tokio::time::Instant::now()) {
        let _ = write_typed_frame(&mut send, &PairingMessage::Rejected).await;
        return Ok(None);
    }

    let request = PairingRequestSummary {
        request_id: proof.request_id,
        device_name: client_name.clone(),
        address: remote,
        fingerprint: DeviceId::from_certificate(&client_material.certificate),
        code: proof.code,
        expires_at_unix_seconds: unix_timestamp()?
            .saturating_add(PAIRING_REQUEST_LIFETIME.as_secs()),
    };
    publish_status(events, RuntimeStatus::PairingPending).await;
    events
        .send(AppEvent::PairingRequested(Box::new(request)))
        .await;
    let deadline = tokio::time::Instant::now() + PAIRING_REQUEST_LIFETIME;
    let outcome = loop {
        tokio::select! {
            decision = decisions.recv() => match decision {
                Some(decision) if decision.request_id == proof.request_id => {
                    break if decision.accepted {
                        PairingOutcome::Accepted
                    } else {
                        PairingOutcome::Rejected
                    }
                },
                Some(_) => continue,
                None => break PairingOutcome::Rejected,
            },
            _ = tokio::time::sleep_until(deadline) => break PairingOutcome::Expired,
            _ = wait_for_stop(stop) => break PairingOutcome::Stopped,
            _ = connection.closed() => break PairingOutcome::Stopped,
        }
    };
    events
        .send(if outcome == PairingOutcome::Expired {
            AppEvent::PairingExpired(proof.request_id)
        } else {
            AppEvent::PairingCleared(proof.request_id)
        })
        .await;
    if outcome != PairingOutcome::Accepted {
        let _ = write_typed_frame(&mut send, &PairingMessage::Rejected).await;
        return Ok(None);
    }
    let device_id = trust.remember(
        client_name.clone(),
        &client_material.certificate,
        unix_timestamp()?,
    )?;
    write_typed_frame(&mut send, &PairingMessage::Accepted).await?;
    expect_pairing_acknowledgement(&mut receive).await?;
    events
        .send(AppEvent::PeerTrusted(PeerSummary {
            device_id,
            display_name: client_name.clone(),
        }))
        .await;
    send.finish().context("finish pairing stream")?;
    Ok(Some(AuthorizedPeer {
        summary: PeerSummary {
            device_id,
            display_name: client_name,
        },
        newly_trusted: true,
    }))
}

struct AuthorizedPeer {
    summary: PeerSummary,
    newly_trusted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PairingOutcome {
    Accepted,
    Rejected,
    Expired,
    Stopped,
}

#[derive(Default)]
struct PairingRateLimiter {
    attempts: VecDeque<(tokio::time::Instant, IpAddr)>,
}

impl PairingRateLimiter {
    fn allow(&mut self, source: IpAddr, now: tokio::time::Instant) -> bool {
        let window = Duration::from_secs(60);
        while self
            .attempts
            .front()
            .is_some_and(|(attempt, _)| now.duration_since(*attempt) >= window)
        {
            self.attempts.pop_front();
        }
        let source_attempts = self
            .attempts
            .iter()
            .filter(|(_, address)| *address == source)
            .count();
        if self.attempts.len() >= 20 || source_attempts >= 5 {
            return false;
        }
        self.attempts.push_back((now, source));
        true
    }
}

async fn expect_pairing_acknowledgement(receive: &mut RecvStream) -> Result<()> {
    match read_typed_frame::<PairingMessage>(receive).await? {
        PairingMessage::Acknowledged => Ok(()),
        _ => bail!("client did not acknowledge persisted pairing state"),
    }
}

fn validate_pairing_hello(
    message: PairingMessage,
    expected_role: PairingRole,
    certificate: Vec<u8>,
) -> Result<(String, PairingMaterial)> {
    let PairingMessage::Hello {
        version,
        role,
        device_name,
        nonce,
        certificate_fingerprint,
    } = message
    else {
        bail!("first pairing message must be Hello")
    };
    if version != PAIRING_PROTOCOL_VERSION {
        bail!("unsupported pairing protocol version {version}");
    }
    if role != expected_role {
        bail!("pairing peer sent the wrong role");
    }
    if certificate_fingerprint != DeviceId::from_certificate(&certificate) {
        bail!("pairing certificate fingerprint does not match TLS identity");
    }
    if device_name.trim().is_empty() || device_name.len() > 128 {
        bail!("pairing device name must contain 1 to 128 bytes");
    }
    Ok((
        device_name,
        PairingMaterial {
            role,
            certificate,
            nonce,
        },
    ))
}

fn unix_timestamp() -> Result<u64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_secs())
}

#[derive(Clone, Copy)]
struct LocalInputSources<'a> {
    current: Option<usize>,
    held: &'a HashMap<HeldInput, usize>,
}

fn held_input(event: InputEvent) -> Option<HeldInput> {
    match event {
        InputEvent::Key { key, .. } => Some(HeldInput::Key(key)),
        InputEvent::Button { button, .. } => Some(HeldInput::Button(button)),
        InputEvent::Scroll { .. } => None,
    }
}

fn input_state(event: InputEvent) -> Option<ButtonState> {
    match event {
        InputEvent::Key { state, .. } | InputEvent::Button { state, .. } => Some(state),
        InputEvent::Scroll { .. } => None,
    }
}

fn local_event_source(
    event: InputEvent,
    current_source: Option<usize>,
    held_sources: &HashMap<HeldInput, usize>,
) -> usize {
    held_input(event)
        .and_then(|held| held_sources.get(&held).copied())
        .or(current_source)
        .unwrap_or(0)
}

async fn run_client(
    mut config: ClientConfig,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let detected_screens = platform::screens()?;
    let size = config.size.unwrap_or(
        detected_screens
            .iter()
            .find(|screen| screen.primary)
            .or_else(|| detected_screens.first())
            .context("platform reported no active screens")?
            .logical_size,
    );
    config.size = Some(size);
    let target = config.target.resolve().await?;
    let bind_ip = if target.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let identity = IdentityPaths {
        certificate: config.identity_cert.clone(),
        private_key: config.identity_key.clone(),
    };
    let local_device_id = DeviceId::from_certificate(
        &fs::read(&config.identity_cert).context("read client identity certificate")?,
    );
    let topology_path = crate::topology_store::default_path()?;
    let state = crate::topology_store::load_state(&topology_path)?;
    let inventory = reconcile_discovered_inventory(
        state.inventory,
        &config.device_name,
        local_device_id,
        &detected_screens,
    );
    crate::topology_store::save_inventory(&topology_path, &inventory)?;
    let topology = state
        .layout
        .resolve(&inventory)
        .map_err(anyhow::Error::msg)?;
    events.send(AppEvent::TopologyChanged(topology)).await;
    let endpoint = transport::pairing_client_endpoint(SocketAddr::new(bind_ip, 0), &identity)?;
    let mut trust = TrustStore::load(&config.trust_store)?;
    let retry_deadline = tokio::time::Instant::now() + config.retry_for;
    let mut attempt = 0_u32;
    loop {
        attempt = attempt.saturating_add(1);
        diagnostics.emit(DiagnosticEvent::ConnectionAttempt {
            attempt,
            remote: target,
        });
        publish_status(&events, RuntimeStatus::Connecting).await;
        let result = run_client_once(
            &endpoint,
            target,
            &config,
            &mut trust,
            ClientAttempt {
                stop: stop.clone(),
                events: events.clone(),
                diagnostics: diagnostics.clone(),
                detected_screens: detected_screens.clone(),
            },
        )
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(error) if tokio::time::Instant::now() < retry_deadline => {
                tracing::warn!(%error, remote = %target, "client session ended; retrying");
                events.try_send(AppEvent::PeerChanged(None));
                publish_status(&events, RuntimeStatus::Retrying).await;
                let remaining =
                    retry_deadline.saturating_duration_since(tokio::time::Instant::now());
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(1).min(remaining)) => {}
                    _ = wait_for_stop(&mut stop) => return Ok(()),
                }
                if tokio::time::Instant::now() >= retry_deadline {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn reconcile_discovered_inventory(
    mut inventory: ScreenInventory,
    device_name: &str,
    device_id: DeviceId,
    screens: &[platform::DetectedScreen],
) -> ScreenInventory {
    for node in &mut inventory.screens {
        node.online = false;
        node.this_device = false;
    }
    merge_local_inventory(&mut inventory, device_name, device_id, screens);
    inventory
}

fn merge_local_inventory(
    inventory: &mut ScreenInventory,
    device_name: &str,
    device_id: DeviceId,
    screens: &[platform::DetectedScreen],
) {
    let device_id = TopologyDeviceId(device_id.to_string());
    for node in inventory
        .screens
        .iter_mut()
        .filter(|node| node.device_id == device_id)
    {
        node.online = false;
        node.this_device = true;
    }
    for screen in screens {
        let screen_id = ScreenId(format!("{}/{}", device_id.0, screen.stable_id));
        if let Some(node) = inventory
            .screens
            .iter_mut()
            .find(|node| node.screen_id == screen_id && node.device_id == device_id)
        {
            node.device_name = device_name.to_owned();
            node.name = screen.name.clone();
            node.logical_size = screen.logical_size;
            node.online = true;
            node.this_device = true;
        } else {
            inventory.screens.push(ScreenDescriptor {
                screen_id,
                device_id: device_id.clone(),
                device_name: device_name.to_owned(),
                name: screen.name.clone(),
                logical_size: screen.logical_size,
                primary: screen.primary,
                online: true,
                this_device: true,
            });
        }
    }
}

fn wire_screens(screens: &[platform::DetectedScreen]) -> Vec<WireScreenDescriptor> {
    screens
        .iter()
        .map(|screen| WireScreenDescriptor {
            stable_id: screen.stable_id.clone(),
            name: screen.name.clone(),
            width: screen.logical_size.width,
            height: screen.logical_size.height,
            primary: screen.primary,
        })
        .collect()
}

fn validate_wire_screens(screens: Vec<WireScreenDescriptor>) -> Result<Vec<WireScreenDescriptor>> {
    if screens.is_empty() || screens.len() > 64 {
        bail!("peer must report between 1 and 64 screens");
    }
    let mut ids = std::collections::HashSet::new();
    for screen in &screens {
        if screen.stable_id.trim().is_empty()
            || screen.stable_id.len() > 256
            || screen.name.trim().is_empty()
            || screen.name.len() > 256
        {
            bail!("peer screen IDs and names must contain 1 to 256 bytes");
        }
        if !ids.insert(&screen.stable_id) {
            bail!("peer reported duplicate screen ID {}", screen.stable_id);
        }
        ScreenSize::new(screen.width, screen.height).map_err(anyhow::Error::msg)?;
    }
    Ok(screens)
}

fn merge_remote_inventory(
    inventory: &mut ScreenInventory,
    peer: &PeerSummary,
    screens: &[WireScreenDescriptor],
) {
    let device_id = TopologyDeviceId(peer.device_id.to_string());
    for screen in inventory
        .screens
        .iter_mut()
        .filter(|screen| screen.device_id == device_id)
    {
        screen.online = false;
        screen.this_device = false;
    }
    for screen in screens {
        let screen_id = ScreenId(format!("{}/{}", device_id.0, screen.stable_id));
        let logical_size = ScreenSize::new(screen.width, screen.height)
            .expect("wire screens are validated before inventory merge");
        if let Some(existing) = inventory
            .screens
            .iter_mut()
            .find(|existing| existing.screen_id == screen_id)
        {
            existing.device_id = device_id.clone();
            existing.device_name = peer.display_name.clone();
            existing.name = screen.name.clone();
            existing.logical_size = logical_size;
            existing.primary = screen.primary;
            existing.online = true;
            existing.this_device = false;
        } else {
            inventory.screens.push(ScreenDescriptor {
                screen_id,
                device_id: device_id.clone(),
                device_name: peer.display_name.clone(),
                name: screen.name.clone(),
                logical_size,
                primary: screen.primary,
                online: true,
                this_device: false,
            });
        }
    }
}

fn placement_request(
    newly_trusted: bool,
    peer: &PeerSummary,
    inventory: &ScreenInventory,
    topology: &ScreenTopology,
) -> Result<Option<PlacementRequestSummary>> {
    if !newly_trusted {
        return Ok(None);
    }
    let anchor = inventory
        .screens
        .iter()
        .find(|screen| screen.this_device && screen.online && screen.primary)
        .or_else(|| {
            inventory
                .screens
                .iter()
                .find(|screen| screen.this_device && screen.online)
        })
        .context("no online local screen is available for peer placement")?;
    let peer_device_id = TopologyDeviceId(peer.device_id.to_string());
    let screens = inventory
        .screens
        .iter()
        .filter(|screen| {
            screen.device_id == peer_device_id
                && screen.online
                && !topology.links.iter().any(|link| {
                    link.from.screen_id == screen.screen_id || link.to.screen_id == screen.screen_id
                })
        })
        .map(|screen| {
            let options = topology
                .placement_availability(&anchor.screen_id, &screen.screen_id)
                .map_err(anyhow::Error::msg)?
                .into_iter()
                .map(|availability| PlacementOptionSummary {
                    position: availability.position,
                    occupied_by: availability
                        .occupied_by
                        .into_iter()
                        .map(|screen_id| {
                            topology
                                .screens
                                .iter()
                                .find(|screen| screen.screen_id == screen_id)
                                .map(|screen| format!("{}/{}", screen.device_name, screen.name))
                                .unwrap_or(screen_id.0)
                        })
                        .collect(),
                })
                .collect();
            Ok(PlacementScreenSummary {
                screen_id: screen.screen_id.clone(),
                name: screen.name.clone(),
                options,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if screens.is_empty() {
        return Ok(None);
    }
    Ok(Some(PlacementRequestSummary {
        device_name: peer.display_name.clone(),
        revision: topology.revision,
        anchor_screen: anchor.screen_id.clone(),
        anchor_name: format!("{}/{}", anchor.device_name, anchor.name),
        screens,
    }))
}

fn migrate_legacy_direction(
    layout: &crate::core::ScreenLayout,
    inventory: &ScreenInventory,
    direction: ScreenDirection,
) -> Result<crate::core::ScreenLayout> {
    let local = inventory
        .screens
        .iter()
        .find(|screen| screen.this_device && screen.primary)
        .or_else(|| inventory.screens.iter().find(|screen| screen.this_device))
        .context("cannot migrate direction without a local screen")?;
    let remotes: Vec<_> = inventory
        .screens
        .iter()
        .filter(|screen| !screen.this_device && screen.online)
        .collect();
    let [remote] = remotes.as_slice() else {
        bail!("legacy --direction migration requires exactly one online remote screen")
    };
    let position = match direction {
        ScreenDirection::Left => crate::core::RelativePosition::LeftOf,
        ScreenDirection::Right => crate::core::RelativePosition::RightOf,
        ScreenDirection::Top => crate::core::RelativePosition::Above,
        ScreenDirection::Bottom => crate::core::RelativePosition::Below,
        ScreenDirection::TopRight
        | ScreenDirection::BottomRight
        | ScreenDirection::BottomLeft
        | ScreenDirection::TopLeft => {
            bail!("diagonal --direction cannot be migrated to edge topology")
        }
    };
    layout
        .apply(
            inventory,
            LayoutCommand::Place {
                screen_id: remote.screen_id.clone(),
                anchor_id: local.screen_id.clone(),
                position,
                replace: false,
            },
        )
        .map_err(anyhow::Error::msg)
}

struct ClientAttempt {
    stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
    detected_screens: Vec<platform::DetectedScreen>,
}

async fn run_client_once(
    endpoint: &Endpoint,
    remote: SocketAddr,
    config: &ClientConfig,
    trust: &mut TrustStore,
    attempt: ClientAttempt,
) -> Result<()> {
    let ClientAttempt {
        mut stop,
        events,
        diagnostics,
        detected_screens,
    } = attempt;
    let connection = tokio::select! {
        connection = transport::connect(endpoint, remote) => connection?,
        _ = wait_for_stop(&mut stop) => return Ok(()),
    };
    let Some(server_peer) =
        authorize_server_peer(&connection, config, trust, &events, &mut stop).await?
    else {
        return Ok(());
    };
    tracing::info!(%remote, screens = detected_screens.len(), "connected to host");
    events.send(AppEvent::PeerChanged(Some(remote))).await;
    events.try_send(AppEvent::ControlChanged(ControlTarget::Remote));
    publish_status(&events, RuntimeStatus::Connected).await;

    let mut metadata = connection
        .open_uni()
        .await
        .context("open client metadata stream")?;
    write_reliable(
        &mut metadata,
        &ReliableEvent::ClientHello {
            version: PROTOCOL_VERSION,
            screens: wire_screens(&detected_screens),
        },
    )
    .await?;
    let mut stream = connection
        .accept_uni()
        .await
        .context("accept host input stream")?;
    let host_screens = match read_reliable(&mut stream).await? {
        ReliableEvent::Hello {
            version: PROTOCOL_VERSION,
            screens,
        } => validate_wire_screens(screens)?,
        ReliableEvent::Hello { version, .. } => {
            bail!("unsupported host protocol version {version}")
        }
        _ => bail!("first host message must be Hello"),
    };
    let topology_path = crate::topology_store::default_path()?;
    let state = crate::topology_store::load_state(&topology_path)?;
    let mut inventory = state.inventory;
    merge_remote_inventory(&mut inventory, &server_peer, &host_screens);
    crate::topology_store::save_inventory(&topology_path, &inventory)?;
    events
        .send(AppEvent::TopologyChanged(
            state
                .layout
                .resolve(&inventory)
                .map_err(anyhow::Error::msg)?,
        ))
        .await;
    let result = run_client_connection(
        connection,
        stream,
        ClientConnectionContext {
            metadata,
            known_screens: wire_screens(&detected_screens),
            server_peer: server_peer.clone(),
            topology_path: topology_path.clone(),
            stop,
            events: events.clone(),
            diagnostics,
        },
    )
    .await;
    let state = crate::topology_store::load_state(&topology_path)?;
    let mut inventory = state.inventory;
    let server_device_id = TopologyDeviceId(server_peer.device_id.to_string());
    for screen in inventory
        .screens
        .iter_mut()
        .filter(|screen| screen.device_id == server_device_id)
    {
        screen.online = false;
    }
    crate::topology_store::save_inventory(&topology_path, &inventory)?;
    events
        .send(AppEvent::TopologyChanged(
            state
                .layout
                .resolve(&inventory)
                .map_err(anyhow::Error::msg)?,
        ))
        .await;
    result
}

async fn authorize_server_peer(
    connection: &Connection,
    config: &ClientConfig,
    trust: &mut TrustStore,
    events: &AppEventBus,
    stop: &mut watch::Receiver<bool>,
) -> Result<Option<PeerSummary>> {
    let remote = connection.remote_address();
    let peer_certificate = transport::peer_certificate(connection)?;
    let local_certificate =
        fs::read(&config.identity_cert).context("read client identity certificate")?;
    let client_material = PairingMaterial::generate(PairingRole::Client, local_certificate)?;
    let (mut send, mut receive) = connection.open_bi().await.context("open pairing stream")?;
    write_typed_frame(
        &mut send,
        &client_material.hello(config.device_name.clone()),
    )
    .await?;
    let server_message: PairingMessage = tokio::select! {
        message = read_typed_frame(&mut receive) => message?,
        _ = wait_for_stop(stop) => return Ok(None),
    };
    let (server_name, server_material) =
        validate_pairing_hello(server_message, PairingRole::Server, peer_certificate)?;
    events
        .send(AppEvent::PeerIdentified(PeerSummary {
            device_id: DeviceId::from_certificate(&server_material.certificate),
            display_name: server_name.clone(),
        }))
        .await;
    let proof = pairing_proof(&server_material, &client_material)?;
    let endpoint = config.target.to_string();
    let verification = trust.verify_endpoint(&endpoint, &server_material.certificate);
    if let VerifyPeer::IdentityChanged { expected, observed } = verification {
        bail!(
            "SECURITY ERROR: identity for {endpoint} changed; expected {expected}, received {observed}; forget the old peer before pairing again"
        );
    }
    if let Some(pinned_path) = &config.server_cert {
        let pinned = fs::read(pinned_path).context("read pinned server certificate")?;
        if pinned != server_material.certificate {
            bail!("SECURITY ERROR: server certificate does not match --server-cert");
        }
    }
    if matches!(verification, VerifyPeer::Unknown(_)) {
        events
            .send(AppEvent::PairingCodeReady(Box::new(
                PairingRequestSummary {
                    request_id: proof.request_id,
                    device_name: server_name.clone(),
                    address: remote,
                    fingerprint: DeviceId::from_certificate(&server_material.certificate),
                    code: proof.code,
                    expires_at_unix_seconds: unix_timestamp()?
                        .saturating_add(PAIRING_REQUEST_LIFETIME.as_secs()),
                },
            )))
            .await;
    }
    let decision: PairingMessage = tokio::select! {
        message = read_typed_frame(&mut receive) => message?,
        _ = tokio::time::sleep(PAIRING_REQUEST_LIFETIME) => bail!("pairing request expired"),
        _ = wait_for_stop(stop) => return Ok(None),
    };
    if matches!(verification, VerifyPeer::Unknown(_)) {
        events
            .send(AppEvent::PairingCleared(proof.request_id))
            .await;
    }
    match decision {
        PairingMessage::Accepted => {}
        PairingMessage::Rejected => bail!("pairing request rejected by server"),
        PairingMessage::Hello { .. } | PairingMessage::Acknowledged => {
            bail!("server sent an invalid pairing decision")
        }
    }
    let device_id = if matches!(verification, VerifyPeer::Unknown(_)) {
        let device_id = trust.remember(
            server_name.clone(),
            &server_material.certificate,
            unix_timestamp()?,
        )?;
        trust.bind_endpoint(endpoint, device_id)?;
        events
            .send(AppEvent::PeerTrusted(PeerSummary {
                device_id,
                display_name: server_name.clone(),
            }))
            .await;
        device_id
    } else {
        DeviceId::from_certificate(&server_material.certificate)
    };
    write_typed_frame(&mut send, &PairingMessage::Acknowledged).await?;
    send.finish().context("finish pairing acknowledgement")?;
    Ok(Some(PeerSummary {
        device_id,
        display_name: server_name,
    }))
}

struct ClientConnectionContext {
    metadata: SendStream,
    known_screens: Vec<WireScreenDescriptor>,
    server_peer: PeerSummary,
    topology_path: PathBuf,
    stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
}

async fn run_client_connection(
    connection: Connection,
    mut stream: RecvStream,
    context: ClientConnectionContext,
) -> Result<()> {
    let ClientConnectionContext {
        mut metadata,
        mut known_screens,
        server_peer,
        topology_path,
        mut stop,
        events,
        diagnostics,
    } = context;
    let mut injector = InputInjector::new(&[])?;
    let mut session = DesktopSession::client();
    let mut inventory_poll = tokio::time::interval(Duration::from_secs(2));
    inventory_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                _ = inventory_poll.tick() => {
                    let current = match platform::screens() {
                        Ok(screens) => wire_screens(&screens),
                        Err(error) => {
                            tracing::warn!(%error, "could not refresh local screen inventory");
                            continue;
                        }
                    };
                    if current != known_screens {
                        write_reliable(
                            &mut metadata,
                            &ReliableEvent::ScreenInventory { screens: current.clone() },
                        ).await?;
                        known_screens = current;
                    }
                }
                datagram = connection.read_datagram() => {
                    let bytes = datagram.context("read pointer datagram")?;
                    let motion: MotionDto = decode(&bytes)?;
                    let sequence = motion.sequence;
                    let effects = session.handle(SessionEvent::RemoteMotion(Motion::from(motion)));
                    if effects.is_empty() {
                        diagnostics.emit(DiagnosticEvent::MotionDropped { sequence });
                    }
                    execute_client_effects(effects, &mut injector)?;
                }
                event = read_reliable(&mut stream) => match event? {
                    event @ ReliableEvent::Input { .. } => {
                        execute_client_effects(
                            session.handle(SessionEvent::RemoteInput(decode_input(&event)?)),
                            &mut injector,
                        )?;
                    }
                    ReliableEvent::EnterScreen { x, y } => {
                        execute_client_effects(
                            session.handle(SessionEvent::EnterRemote { x, y }),
                            &mut injector,
                        )?;
                    }
                    ReliableEvent::ReleaseAll => {
                        execute_client_effects(
                            session.handle(SessionEvent::ReleaseRemote),
                            &mut injector,
                        )?;
                    }
                    ReliableEvent::Heartbeat { .. } => {}
                    ReliableEvent::ScreenInventory { screens } => {
                        let screens = validate_wire_screens(screens)?;
                        let state = crate::topology_store::load_state(&topology_path)?;
                        let mut inventory = state.inventory;
                        merge_remote_inventory(&mut inventory, &server_peer, &screens);
                        crate::topology_store::save_inventory(&topology_path, &inventory)?;
                        events.send(AppEvent::TopologyChanged(
                            state.layout.resolve(&inventory).map_err(anyhow::Error::msg)?,
                        )).await;
                    }
                    ReliableEvent::Hello { .. }
                    | ReliableEvent::ClientHello { .. } => {
                        bail!("unexpected handshake message")
                    }
                },
                _ = wait_for_stop(&mut stop) => break,
            }
        }
        Ok(())
    }
    .await;
    let cleanup = execute_client_effects(
        session.handle(if result.is_ok() {
            SessionEvent::StopRequested
        } else {
            SessionEvent::PeerDisconnected
        }),
        &mut injector,
    );
    cleanup?;
    result
}

fn execute_client_effects(effects: Vec<SessionEffect>, injector: &mut InputInjector) -> Result<()> {
    for effect in effects {
        match effect {
            SessionEffect::InjectLocal(event) => injector.emit(0, event)?,
            SessionEffect::InjectLocalMotion { dx, dy } => injector.emit_motion(0, dx, dy)?,
            SessionEffect::SetLocalCursor { x, y } => injector.set_cursor_position(x, y)?,
            SessionEffect::SendRemote(_)
            | SessionEffect::SendRemoteMotion(_)
            | SessionEffect::EnterRemote { .. }
            | SessionEffect::ReleaseRemote
            | SessionEffect::SendScreen { .. }
            | SessionEffect::SendScreenMotion { .. }
            | SessionEffect::EnterScreen { .. }
            | SessionEffect::ReleaseScreen { .. }
            | SessionEffect::ControlChanged(_) => {
                bail!("host-only effect produced by client session")
            }
        }
    }
    Ok(())
}

async fn wait_for_stop(stop: &mut watch::Receiver<bool>) {
    if *stop.borrow() {
        return;
    }
    let _ = stop.changed().await;
}

async fn write_reliable(stream: &mut SendStream, event: &ReliableEvent) -> Result<()> {
    stream
        .write_all(&encode_frame(event)?)
        .await
        .context("write reliable input event")
}

async fn write_typed_frame<T: serde::Serialize>(stream: &mut SendStream, value: &T) -> Result<()> {
    let body = encode(value)?;
    let length = u32::try_from(body.len()).context("pairing frame is too large")?;
    stream
        .write_all(&length.to_be_bytes())
        .await
        .context("write pairing frame header")?;
    stream
        .write_all(&body)
        .await
        .context("write pairing frame body")
}

async fn read_typed_frame<T: DeserializeOwned>(stream: &mut RecvStream) -> Result<T> {
    let mut header = [0_u8; 4];
    stream
        .read_exact(&mut header)
        .await
        .context("read pairing frame header")?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_RELIABLE_FRAME {
        bail!("pairing frame length {length} exceeds limit");
    }
    let mut body = vec![0_u8; length];
    stream
        .read_exact(&mut body)
        .await
        .context("read pairing frame body")?;
    decode(&body)
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

    fn test_event_bus() -> (AppEventBus, mpsc::Receiver<AppEvent>) {
        let (sender, receiver) = mpsc::channel(32);
        (
            AppEventBus {
                sender,
                snapshot: Arc::new(std::sync::RwLock::new(RuntimeSnapshot::default())),
            },
            receiver,
        )
    }

    #[tokio::test]
    async fn idle_runtime_stops_cleanly() {
        let mut runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
        runtime.send(AppCommand::Stop).await.unwrap();
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Stopped))
        );
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn configuration_is_reflected_in_the_authoritative_snapshot() {
        let mut runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
        let config = AppConfig {
            host: None,
            client: Some(ClientConfig {
                target: "127.0.0.1:24801".parse().unwrap(),
                identity_cert: PathBuf::from("identity-cert.der"),
                identity_key: PathBuf::from("identity-key.der"),
                server_cert: Some(PathBuf::from("server-cert.der")),
                size: Some(ScreenSize::new(100, 100).unwrap()),
                retry_for: Duration::from_secs(30),
                device_name: "client".to_owned(),
                trust_store: PathBuf::from("trust.postcard"),
            }),
        };
        runtime
            .send(AppCommand::UpdateConfig(config.clone()))
            .await
            .unwrap();
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::ConfigChanged(Box::new(config.clone())))
        );
        assert_eq!(runtime.snapshot().config, config);
        runtime.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn invalid_start_has_stable_status_and_fault_events() {
        let mut runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
        runtime
            .send(AppCommand::StartHost(HostConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert: PathBuf::from("missing-cert"),
                key: PathBuf::from("missing-key"),
                size: Some(ScreenSize::new(100, 100).unwrap()),
                devices: Vec::new(),
                direction: None,
                device_name: "host".to_owned(),
                trust_store: PathBuf::from("trust.postcard"),
            }))
            .await
            .unwrap();
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Starting))
        );
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::PeerChanged(None))
        );
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Faulted))
        );
        let Some(AppEvent::Faulted(fault)) = runtime.next_event().await else {
            panic!("expected classified fault")
        };
        assert_eq!(fault.kind, FaultKind::InvalidConfiguration);
        assert!(fault.message.contains("identity certificate"));
        assert_eq!(
            runtime.snapshot(),
            RuntimeSnapshot {
                status: RuntimeStatus::Faulted,
                peer: None,
                peer_device: None,
                control: None,
                config: AppConfig::default(),
                fault: Some(fault),
                pairing: None,
                topology: ScreenTopology::default(),
            }
        );
        runtime.shutdown().await.unwrap();
    }

    #[cfg(not(target_os = "windows"))]
    #[tokio::test]
    async fn shutdown_terminates_an_active_supervisor() {
        let directory = tempfile::tempdir().unwrap();
        let cert = directory.path().join("cert.der");
        let key = directory.path().join("key.der");
        transport::generate_identity(&cert, &key, false).unwrap();
        let mut runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
        runtime
            .send(AppCommand::StartHost(HostConfig {
                bind: "127.0.0.1:0".parse().unwrap(),
                cert,
                key,
                size: Some(ScreenSize::new(100, 100).unwrap()),
                devices: vec![PathBuf::from("/dev/null")],
                direction: Some(ScreenDirection::Right),
                device_name: "host".to_owned(),
                trust_store: directory.path().join("trust.postcard"),
            }))
            .await
            .unwrap();
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Starting))
        );
        assert!(matches!(
            runtime.next_event().await,
            Some(AppEvent::TopologyChanged(_))
        ));
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Listening))
        );
        tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
            .await
            .expect("active runtime shutdown timed out")
            .unwrap();
    }

    #[tokio::test]
    async fn unknown_devices_wait_for_confirmation_then_persist_both_sides() {
        let directory = tempfile::tempdir().unwrap();
        let server_identity = IdentityPaths::in_directory(directory.path().join("server-identity"));
        let client_identity = IdentityPaths::in_directory(directory.path().join("client-identity"));
        crate::identity::ensure_identity(&server_identity).unwrap();
        crate::identity::ensure_identity(&client_identity).unwrap();
        let server_certificate = fs::read(&server_identity.certificate).unwrap();
        let client_certificate = fs::read(&client_identity.certificate).unwrap();
        let server_trust_path = directory.path().join("server-trust");
        let client_trust_path = directory.path().join("client-trust");
        let server_endpoint =
            transport::pairing_server_endpoint("127.0.0.1:0".parse().unwrap(), &server_identity)
                .unwrap();
        let remote = server_endpoint.local_addr().unwrap();
        let client_endpoint =
            transport::pairing_client_endpoint("0.0.0.0:0".parse().unwrap(), &client_identity)
                .unwrap();
        let server_config = HostConfig {
            bind: "127.0.0.1:0".parse().unwrap(),
            cert: server_identity.certificate,
            key: server_identity.private_key,
            size: Some(ScreenSize::new(100, 100).unwrap()),
            devices: Vec::new(),
            direction: Some(ScreenDirection::Right),
            device_name: "linux-desktop".to_owned(),
            trust_store: server_trust_path.clone(),
        };
        let client_config = ClientConfig {
            target: "linux-desktop.local".parse().unwrap(),
            identity_cert: client_identity.certificate,
            identity_key: client_identity.private_key,
            server_cert: None,
            size: Some(ScreenSize::new(100, 100).unwrap()),
            retry_for: Duration::ZERO,
            device_name: "macmini".to_owned(),
            trust_store: client_trust_path.clone(),
        };
        let reconnect_server_config = server_config.clone();
        let reconnect_client_config = client_config.clone();
        let (server_events, mut server_event_rx) = test_event_bus();
        let (client_events, mut client_event_rx) = test_event_bus();
        let (decision_tx, mut decision_rx) = mpsc::channel(1);
        let (server_stop_tx, mut server_stop) = watch::channel(false);
        let (client_stop_tx, mut client_stop) = watch::channel(false);

        let server_task = tokio::spawn(async move {
            let connection = transport::accept_one(&server_endpoint).await.unwrap();
            let mut trust = TrustStore::load(&server_config.trust_store).unwrap();
            let mut rate_limiter = PairingRateLimiter::default();
            let authorized = authorize_host_peer(
                &connection,
                &server_config,
                &mut trust,
                &mut rate_limiter,
                &mut decision_rx,
                &mut server_stop,
                &server_events,
            )
            .await
            .unwrap();
            (authorized, connection, server_endpoint)
        });
        let connection = transport::connect(&client_endpoint, remote).await.unwrap();
        let client_task = tokio::spawn(async move {
            let mut trust = TrustStore::load(&client_config.trust_store).unwrap();
            let authorized = authorize_server_peer(
                &connection,
                &client_config,
                &mut trust,
                &client_events,
                &mut client_stop,
            )
            .await
            .unwrap();
            (authorized, connection)
        });

        let request = loop {
            if let AppEvent::PairingRequested(request) = server_event_rx.recv().await.unwrap() {
                break request;
            }
        };
        let client_notice = loop {
            if let AppEvent::PairingCodeReady(request) = client_event_rx.recv().await.unwrap() {
                break request;
            }
        };
        assert_eq!(request.request_id, client_notice.request_id);
        assert_eq!(request.code, client_notice.code);
        assert!(!server_task.is_finished());
        decision_tx
            .send(PairingDecision {
                request_id: request.request_id,
                accepted: true,
            })
            .await
            .unwrap();
        let (server_authorized, first_server_connection, first_server_endpoint) =
            server_task.await.unwrap();
        assert!(server_authorized.is_some());
        let (client_authorized, first_connection) = client_task.await.unwrap();
        assert!(client_authorized.is_some());
        first_connection.close(0_u32.into(), b"test pairing complete");
        first_server_connection.close(0_u32.into(), b"test pairing complete");
        first_server_endpoint.close(0_u32.into(), b"test pairing complete");
        drop(first_connection);
        client_endpoint.close(0_u32.into(), b"test pairing complete");
        first_server_endpoint.wait_idle().await;
        client_endpoint.wait_idle().await;
        drop(first_server_connection);
        drop(first_server_endpoint);
        drop(client_endpoint);
        drop((server_stop_tx, client_stop_tx));

        let server_trust = TrustStore::load(server_trust_path).unwrap();
        assert!(matches!(
            server_trust.verify_certificate(&client_certificate),
            VerifyPeer::Trusted(_)
        ));
        let client_trust = TrustStore::load(client_trust_path).unwrap();
        assert!(matches!(
            client_trust.verify_endpoint("linux-desktop.local:24801", &server_certificate),
            VerifyPeer::Trusted(_)
        ));

        let reconnect_server_identity = IdentityPaths {
            certificate: reconnect_server_config.cert.clone(),
            private_key: reconnect_server_config.key.clone(),
        };
        let reconnect_client_identity = IdentityPaths {
            certificate: reconnect_client_config.identity_cert.clone(),
            private_key: reconnect_client_config.identity_key.clone(),
        };
        let reconnect_server_endpoint = transport::pairing_server_endpoint(
            "127.0.0.1:0".parse().unwrap(),
            &reconnect_server_identity,
        )
        .unwrap();
        let reconnect_remote = reconnect_server_endpoint.local_addr().unwrap();
        let reconnect_client_endpoint = transport::pairing_client_endpoint(
            "0.0.0.0:0".parse().unwrap(),
            &reconnect_client_identity,
        )
        .unwrap();
        let (reconnect_server_events, mut reconnect_server_event_rx) = test_event_bus();
        let (reconnect_client_events, mut reconnect_client_event_rx) = test_event_bus();
        let (unused_decision_tx, mut unused_decision_rx) = mpsc::channel(1);
        let (reconnect_server_stop_tx, mut reconnect_server_stop) = watch::channel(false);
        let (reconnect_client_stop_tx, mut reconnect_client_stop) = watch::channel(false);
        let reconnect_server_task = tokio::spawn(async move {
            let connection = transport::accept_one(&reconnect_server_endpoint)
                .await
                .unwrap();
            let mut trust = TrustStore::load(&reconnect_server_config.trust_store).unwrap();
            let mut limiter = PairingRateLimiter::default();
            let authorized = authorize_host_peer(
                &connection,
                &reconnect_server_config,
                &mut trust,
                &mut limiter,
                &mut unused_decision_rx,
                &mut reconnect_server_stop,
                &reconnect_server_events,
            )
            .await
            .unwrap();
            (authorized, connection)
        });
        let reconnect_connection = transport::connect(&reconnect_client_endpoint, reconnect_remote)
            .await
            .unwrap();
        let reconnect_client_task = tokio::spawn(async move {
            let mut trust = TrustStore::load(&reconnect_client_config.trust_store).unwrap();
            let authorized = authorize_server_peer(
                &reconnect_connection,
                &reconnect_client_config,
                &mut trust,
                &reconnect_client_events,
                &mut reconnect_client_stop,
            )
            .await
            .unwrap();
            (authorized, reconnect_connection)
        });
        let (server_authorized, server_connection) = reconnect_server_task.await.unwrap();
        let (client_authorized, client_connection) = reconnect_client_task.await.unwrap();
        assert!(server_authorized.is_some() && client_authorized.is_some());
        assert!(matches!(
            reconnect_server_event_rx.try_recv(),
            Ok(AppEvent::PeerIdentified(PeerSummary { display_name, .. })) if display_name == "macmini"
        ));
        assert!(matches!(
            reconnect_client_event_rx.try_recv(),
            Ok(AppEvent::PeerIdentified(PeerSummary { display_name, .. })) if display_name == "linux-desktop"
        ));
        assert!(reconnect_server_event_rx.try_recv().is_err());
        assert!(reconnect_client_event_rx.try_recv().is_err());
        drop((
            server_connection,
            client_connection,
            unused_decision_tx,
            reconnect_server_stop_tx,
            reconnect_client_stop_tx,
        ));
    }

    #[test]
    fn faults_have_stable_categories() {
        assert_eq!(
            classify_fault(&anyhow::anyhow!("connection closed")).kind,
            FaultKind::Connection
        );
        assert_eq!(
            classify_fault(&anyhow::anyhow!("grant Accessibility permission")).kind,
            FaultKind::PermissionDenied
        );
    }

    #[test]
    fn held_input_keeps_its_physical_source_across_screen_transitions() {
        let key = crate::core::Key(56);
        let event = InputEvent::Key {
            key,
            state: ButtonState::Released,
        };
        let held_sources = HashMap::from([(HeldInput::Key(key), 3)]);

        assert_eq!(local_event_source(event, Some(1), &held_sources), 3);
    }

    #[test]
    fn refreshed_remote_inventory_marks_unplugged_screens_offline() {
        let peer = PeerSummary {
            device_id: DeviceId([7; 32]),
            display_name: "peer".to_owned(),
        };
        let first = vec![
            WireScreenDescriptor {
                stable_id: "a".to_owned(),
                name: "A".to_owned(),
                width: 1920,
                height: 1080,
                primary: true,
            },
            WireScreenDescriptor {
                stable_id: "b".to_owned(),
                name: "B".to_owned(),
                width: 1280,
                height: 720,
                primary: false,
            },
        ];
        let mut inventory = ScreenInventory::default();
        merge_remote_inventory(&mut inventory, &peer, &first);

        merge_remote_inventory(&mut inventory, &peer, &first[..1]);

        assert!(
            inventory
                .screens
                .iter()
                .any(|screen| { screen.screen_id.0.ends_with("/a") && screen.online })
        );
        assert!(
            inventory
                .screens
                .iter()
                .any(|screen| { screen.screen_id.0.ends_with("/b") && !screen.online })
        );
    }

    #[test]
    fn newly_trusted_peer_requests_placement_after_inventory_is_available() {
        let peer = PeerSummary {
            device_id: DeviceId([7; 32]),
            display_name: "peer-b".to_owned(),
        };
        let inventory = ScreenInventory {
            screens: vec![
                ScreenDescriptor {
                    screen_id: ScreenId("local".into()),
                    device_id: TopologyDeviceId("host-id".into()),
                    device_name: "host".into(),
                    name: "primary".into(),
                    logical_size: ScreenSize::new(100, 100).unwrap(),
                    primary: true,
                    online: true,
                    this_device: true,
                },
                ScreenDescriptor {
                    screen_id: ScreenId("candidate".into()),
                    device_id: TopologyDeviceId(peer.device_id.to_string()),
                    device_name: "peer-b".into(),
                    name: "display".into(),
                    logical_size: ScreenSize::new(100, 100).unwrap(),
                    primary: true,
                    online: true,
                    this_device: false,
                },
                ScreenDescriptor {
                    screen_id: ScreenId("occupied".into()),
                    device_id: TopologyDeviceId("peer-c-id".into()),
                    device_name: "peer-c".into(),
                    name: "display".into(),
                    logical_size: ScreenSize::new(100, 100).unwrap(),
                    primary: true,
                    online: true,
                    this_device: false,
                },
            ],
        };
        let layout = crate::core::ScreenLayout {
            links: vec![crate::core::ScreenLink {
                from: crate::core::ScreenEdge {
                    screen_id: ScreenId("local".into()),
                    edge: crate::core::Edge::Right,
                },
                to: crate::core::ScreenEdge {
                    screen_id: ScreenId("occupied".into()),
                    edge: crate::core::Edge::Left,
                },
            }],
            ..crate::core::ScreenLayout::default()
        };
        let topology = layout.resolve(&inventory).unwrap();

        assert!(
            placement_request(false, &peer, &inventory, &topology)
                .unwrap()
                .is_none()
        );
        let request = placement_request(true, &peer, &inventory, &topology)
            .unwrap()
            .unwrap();
        assert_eq!(request.screens.len(), 1);
        assert_eq!(
            request.screens[0].options[1].occupied_by,
            vec!["peer-c/display"]
        );

        let mut placed_layout = layout;
        placed_layout.links.push(crate::core::ScreenLink {
            from: crate::core::ScreenEdge {
                screen_id: ScreenId("candidate".into()),
                edge: crate::core::Edge::Left,
            },
            to: crate::core::ScreenEdge {
                screen_id: ScreenId("occupied".into()),
                edge: crate::core::Edge::Right,
            },
        });
        let placed_topology = placed_layout.resolve(&inventory).unwrap();
        assert!(
            placement_request(true, &peer, &inventory, &placed_topology)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn pairing_requests_are_limited_per_source_and_recover_after_the_window() {
        let mut limiter = PairingRateLimiter::default();
        let now = tokio::time::Instant::now();
        let source: IpAddr = "192.168.1.20".parse().unwrap();
        for _ in 0..5 {
            assert!(limiter.allow(source, now));
        }
        assert!(!limiter.allow(source, now));
        assert!(limiter.allow(source, now + Duration::from_secs(60)));
    }

    #[test]
    fn pairing_requests_are_limited_globally() {
        let mut limiter = PairingRateLimiter::default();
        let now = tokio::time::Instant::now();
        for host in 1..=20 {
            let source = IpAddr::V4(Ipv4Addr::new(10, 0, 0, host));
            assert!(limiter.allow(source, now));
        }
        assert!(!limiter.allow(IpAddr::V4(Ipv4Addr::new(10, 0, 1, 1)), now));
    }

    #[test]
    fn diagnostics_are_not_required_for_business_behavior() {
        NoopDiagnostics.emit(DiagnosticEvent::MotionDropped { sequence: 4 });
    }
}
