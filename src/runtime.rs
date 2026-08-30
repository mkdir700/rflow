use std::{
    collections::VecDeque,
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
use tokio::sync::{mpsc, watch};

use crate::{
    core::{
        ControlTarget, DesktopSession, Motion, ScreenDirection, ScreenId, ScreenNode, ScreenSize,
        ScreenTopology, SessionEffect, SessionEvent, TopologyDeviceId,
    },
    identity::IdentityPaths,
    pairing::{
        PAIRING_PROTOCOL_VERSION, PairingCode, PairingMaterial, PairingMessage, PairingRequestId,
        PairingRole, pairing_proof,
    },
    platform::{self, CapturedEvent, InputInjector},
    protocol::{
        MAX_RELIABLE_FRAME, MotionDto, PROTOCOL_VERSION, ReliableEvent, decode, decode_input,
        encode, encode_frame, encode_input,
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

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppConfig {
    pub host: Option<HostConfig>,
    pub client: Option<ClientConfig>,
}

#[derive(Debug, Clone)]
/// User intent accepted by the single-writer Runtime supervisor.
/// Commands from one handle are processed in FIFO order.
pub enum AppCommand {
    StartHost(HostConfig),
    StartClient(ClientConfig),
    Stop,
    UpdateConfig(AppConfig),
    AcceptPairing(PairingRequestId),
    RejectPairing(PairingRequestId),
    ReplaceTopology {
        expected_revision: u64,
        topology: ScreenTopology,
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
    ControlChanged(ControlTarget),
    ConfigChanged(Box<AppConfig>),
    PairingRequested(Box<PairingRequestSummary>),
    PairingCodeReady(Box<PairingRequestSummary>),
    PairingCleared(PairingRequestId),
    PairingExpired(PairingRequestId),
    PeerTrusted(PeerSummary),
    TopologyChanged(ScreenTopology),
    Faulted(AppFault),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub peer: Option<SocketAddr>,
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
                    snapshot.control = None;
                    snapshot.fault = None;
                } else if matches!(status, RuntimeStatus::Stopped | RuntimeStatus::Faulted) {
                    snapshot.control = None;
                }
            }
            AppEvent::PeerChanged(peer) => snapshot.peer = *peer,
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
            AppEvent::PeerTrusted(_) => {}
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
}

#[cfg(test)]
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
                AppEvent::StatusChanged(RuntimeStatus::Stopped | RuntimeStatus::Faulted)
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
        };
        publish_status(&events, RuntimeStatus::Starting).await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let (pairing_tx, pairing_rx) = mpsc::channel(8);
        let session_events = events.clone();
        let session_diagnostics = diagnostics.clone();
        let mut task = tokio::spawn(async move {
            match session {
                SessionKind::Host(config) => {
                    run_host(
                        config,
                        stop_rx,
                        pairing_rx,
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
                            Ok(topology) => events.send(AppEvent::TopologyChanged(topology)).await,
                            Err(error) => events.send(AppEvent::Faulted(classify_fault(&error))).await,
                        }
                    }
                    Some(AppCommand::RejectPairing(request_id)) => {
                        let _ = pairing_tx
                            .send(PairingDecision {
                                request_id,
                                accepted: false,
                            })
                            .await;
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
            publish_status(events, RuntimeStatus::Faulted).await;
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
    } else if lower.contains("pass --direction")
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
    events: AppEventBus,
    _diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let direction = config
        .direction
        .context("screen direction is required; pass --direction")?;
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
    config.devices = platform::resolve_capture_devices(&config.devices)?;
    platform::validate_capture(&config.devices)?;
    let identity = IdentityPaths {
        certificate: config.cert.clone(),
        private_key: config.key.clone(),
    };
    let local_device_id = DeviceId::from_certificate(
        &fs::read(&config.cert).context("read host identity certificate")?,
    );
    events
        .send(AppEvent::TopologyChanged(discovered_topology(
            &config.device_name,
            local_device_id,
            detected_screens,
        )))
        .await;
    let endpoint = transport::pairing_server_endpoint(config.bind, &identity)?;
    let mut trust = TrustStore::load(&config.trust_store)?;
    let mut pairing_rate_limiter = PairingRateLimiter::default();
    tracing::info!(local = %endpoint.local_addr()?, "host listening");
    publish_status(&events, RuntimeStatus::Listening).await;
    loop {
        let connection = tokio::select! {
            connection = transport::accept_one(&endpoint) => connection?,
            _ = wait_for_stop(&mut stop) => return Ok(()),
        };
        let remote = connection.remote_address();
        if !authorize_host_peer(
            &connection,
            &config,
            &mut trust,
            &mut pairing_rate_limiter,
            &mut pairing_decisions,
            &mut stop,
            &events,
        )
        .await?
        {
            connection.close(0_u32.into(), b"pairing rejected");
            if *stop.borrow() {
                return Ok(());
            }
            publish_status(&events, RuntimeStatus::Listening).await;
            continue;
        }
        events.send(AppEvent::PeerChanged(Some(remote))).await;
        publish_status(&events, RuntimeStatus::Connected).await;
        tracing::info!(%remote, "client connected");
        return run_host_connection(connection, size, config.devices, direction, stop, events)
            .await;
    }
}

async fn authorize_host_peer(
    connection: &Connection,
    config: &HostConfig,
    trust: &mut TrustStore,
    rate_limiter: &mut PairingRateLimiter,
    decisions: &mut mpsc::Receiver<PairingDecision>,
    stop: &mut watch::Receiver<bool>,
    events: &AppEventBus,
) -> Result<bool> {
    let remote = connection.remote_address();
    let peer_certificate = transport::peer_certificate(connection)?;
    let local_certificate = fs::read(&config.cert).context("read host identity certificate")?;
    let (mut send, mut receive) = tokio::select! {
        stream = connection.accept_bi() => stream.context("accept pairing stream")?,
        _ = wait_for_stop(stop) => return Ok(false),
    };
    let client_message: PairingMessage = read_typed_frame(&mut receive).await?;
    let (client_name, client_material) =
        validate_pairing_hello(client_message, PairingRole::Client, peer_certificate)?;
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
        return Ok(true);
    }

    if !rate_limiter.allow(remote.ip(), tokio::time::Instant::now()) {
        let _ = write_typed_frame(&mut send, &PairingMessage::Rejected).await;
        return Ok(false);
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
        return Ok(false);
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
            display_name: client_name,
        }))
        .await;
    send.finish().context("finish pairing stream")?;
    Ok(true)
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

async fn run_host_connection(
    connection: Connection,
    local_size: ScreenSize,
    devices: Vec<PathBuf>,
    direction: ScreenDirection,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
) -> Result<()> {
    let mut metadata = tokio::select! {
        stream = connection.accept_uni() => stream.context("accept client metadata stream")?,
        _ = wait_for_stop(&mut stop) => return Ok(()),
    };
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

    let mut session = DesktopSession::host(local_size, remote_size, direction);
    events.try_send(AppEvent::ControlChanged(ControlTarget::Local));
    let mut injector = InputInjector::new()?;
    if let Some((x, y)) = platform::cursor_position() {
        session.set_local_position(x, y);
    }
    let mut capture = platform::capture(devices, true)?;
    let mut heartbeat = tokio::time::interval(Duration::from_secs(1));
    let mut heartbeat_sequence = 0_u64;
    let mut reliable_sequence = 0_u64;

    let result: Result<()> = async {
        loop {
            tokio::select! {
                captured = capture.next() => {
                    let event = match captured? {
                        CapturedEvent::Input { event, .. } => SessionEvent::PhysicalInput(event),
                        CapturedEvent::Motion(motion) => SessionEvent::PhysicalMotion(motion),
                    };
                    execute_host_effects(
                        session.handle(event),
                        &connection,
                        &mut remote_stream,
                        &mut injector,
                        &events,
                        &mut reliable_sequence,
                    ).await?;
                }
                _ = heartbeat.tick() => {
                    heartbeat_sequence = heartbeat_sequence.wrapping_add(1);
                    write_reliable(&mut remote_stream, &ReliableEvent::Heartbeat { sequence: heartbeat_sequence }).await?;
                }
                closed = connection.closed() => bail!("client connection closed: {closed}"),
                _ = wait_for_stop(&mut stop) => break,
            }
        }
        Ok(())
    }.await;

    let cleanup = execute_host_effects(
        session.handle(if result.is_ok() {
            SessionEvent::StopRequested
        } else {
            SessionEvent::PeerDisconnected
        }),
        &connection,
        &mut remote_stream,
        &mut injector,
        &events,
        &mut reliable_sequence,
    )
    .await;
    connection.close(0_u32.into(), b"host stopped");
    cleanup?;
    result
}

async fn execute_host_effects(
    effects: Vec<SessionEffect>,
    connection: &Connection,
    remote: &mut SendStream,
    local: &mut InputInjector,
    events: &AppEventBus,
    sequence: &mut u64,
) -> Result<()> {
    for effect in effects {
        match effect {
            SessionEffect::InjectLocal(event) => local.emit(event)?,
            SessionEffect::InjectLocalMotion { dx, dy } => local.emit_motion(dx, dy)?,
            SessionEffect::SendRemote(event) => {
                *sequence = sequence.wrapping_add(1);
                write_reliable(remote, &encode_input(*sequence, event)?).await?;
            }
            SessionEffect::SendRemoteMotion(motion) => {
                connection
                    .send_datagram(Bytes::from(encode(&MotionDto::from(motion))?))
                    .context("send remote pointer motion")?;
            }
            SessionEffect::EnterRemote { x, y } => {
                write_reliable(remote, &ReliableEvent::EnterScreen { x, y }).await?;
            }
            SessionEffect::SetLocalCursor { x, y } => local.set_cursor_position(x, y)?,
            SessionEffect::ReleaseRemote => {
                write_reliable(remote, &ReliableEvent::ReleaseAll).await?;
            }
            SessionEffect::ControlChanged(control) => {
                events.try_send(AppEvent::ControlChanged(control));
            }
        }
    }
    Ok(())
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
    events
        .send(AppEvent::TopologyChanged(discovered_topology(
            &config.device_name,
            local_device_id,
            detected_screens,
        )))
        .await;
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
            stop.clone(),
            events.clone(),
            diagnostics.clone(),
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

fn discovered_topology(
    device_name: &str,
    device_id: DeviceId,
    screens: Vec<platform::DetectedScreen>,
) -> ScreenTopology {
    ScreenTopology {
        revision: 1,
        screens: screens
            .into_iter()
            .map(|screen| ScreenNode {
                screen_id: ScreenId(screen.stable_id),
                device_id: TopologyDeviceId(device_id.to_string()),
                device_name: device_name.to_owned(),
                name: screen.name,
                logical_size: screen.logical_size,
                online: true,
                this_device: true,
            })
            .collect(),
        links: Vec::new(),
    }
}

async fn run_client_once(
    endpoint: &Endpoint,
    remote: SocketAddr,
    config: &ClientConfig,
    trust: &mut TrustStore,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let size = config
        .size
        .context("client screen size was not resolved before connection")?;
    let connection = tokio::select! {
        connection = transport::connect(endpoint, remote) => connection?,
        _ = wait_for_stop(&mut stop) => return Ok(()),
    };
    if !authorize_server_peer(&connection, config, trust, &events, &mut stop).await? {
        return Ok(());
    }
    tracing::info!(%remote, width = size.width, height = size.height, "connected to host");
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
    run_client_connection(connection, stream, stop, diagnostics).await
}

async fn authorize_server_peer(
    connection: &Connection,
    config: &ClientConfig,
    trust: &mut TrustStore,
    events: &AppEventBus,
    stop: &mut watch::Receiver<bool>,
) -> Result<bool> {
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
        _ = wait_for_stop(stop) => return Ok(false),
    };
    let (server_name, server_material) =
        validate_pairing_hello(server_message, PairingRole::Server, peer_certificate)?;
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
        _ = wait_for_stop(stop) => return Ok(false),
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
    if matches!(verification, VerifyPeer::Unknown(_)) {
        let device_id = trust.remember(
            server_name.clone(),
            &server_material.certificate,
            unix_timestamp()?,
        )?;
        trust.bind_endpoint(endpoint, device_id)?;
        events
            .send(AppEvent::PeerTrusted(PeerSummary {
                device_id,
                display_name: server_name,
            }))
            .await;
    }
    write_typed_frame(&mut send, &PairingMessage::Acknowledged).await?;
    send.finish().context("finish pairing acknowledgement")?;
    Ok(true)
}

async fn run_client_connection(
    connection: Connection,
    mut stream: RecvStream,
    mut stop: watch::Receiver<bool>,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let mut injector = InputInjector::new()?;
    let mut session = DesktopSession::client();
    let result: Result<()> = async {
        loop {
            tokio::select! {
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
                    ReliableEvent::Hello { .. } | ReliableEvent::ClientHello { .. } => {
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
            SessionEffect::InjectLocal(event) => injector.emit(event)?,
            SessionEffect::InjectLocalMotion { dx, dy } => injector.emit_motion(dx, dy)?,
            SessionEffect::SetLocalCursor { x, y } => injector.set_cursor_position(x, y)?,
            SessionEffect::SendRemote(_)
            | SessionEffect::SendRemoteMotion(_)
            | SessionEffect::EnterRemote { .. }
            | SessionEffect::ReleaseRemote
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
        assert!(fault.message.contains("pass --direction"));
        assert_eq!(
            runtime.snapshot(),
            RuntimeSnapshot {
                status: RuntimeStatus::Faulted,
                peer: None,
                control: None,
                config: AppConfig::default(),
                fault: Some(fault),
                pairing: None,
                topology: ScreenTopology::default(),
            }
        );
        runtime.shutdown().await.unwrap();
    }

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
        assert!(server_authorized);
        let (client_authorized, first_connection) = client_task.await.unwrap();
        assert!(client_authorized);
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
        assert!(server_authorized && client_authorized);
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
