use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bytes::Bytes;
use quinn::{Connection, Endpoint, RecvStream, SendStream};
use tokio::sync::{mpsc, watch};

use crate::{
    core::{
        ControlTarget, DesktopSession, Motion, ScreenDirection, ScreenSize, SessionEffect,
        SessionEvent,
    },
    platform::{self, CapturedEvent, InputInjector},
    protocol::{
        MAX_RELIABLE_FRAME, MotionDto, PROTOCOL_VERSION, ReliableEvent, decode, decode_input,
        encode, encode_frame, encode_input,
    },
    target::ServerTarget,
    transport,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    pub bind: SocketAddr,
    pub cert: PathBuf,
    pub key: PathBuf,
    pub size: ScreenSize,
    pub devices: Vec<PathBuf>,
    pub direction: Option<ScreenDirection>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientConfig {
    pub target: ServerTarget,
    pub identity_cert: PathBuf,
    pub identity_key: PathBuf,
    pub server_cert: PathBuf,
    pub size: Option<ScreenSize>,
    pub retry_for: Duration,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStatus {
    Stopped,
    Starting,
    Listening,
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
    Faulted(AppFault),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    pub status: RuntimeStatus,
    pub peer: Option<SocketAddr>,
    pub control: Option<ControlTarget>,
    pub config: AppConfig,
    pub fault: Option<AppFault>,
}

impl Default for RuntimeSnapshot {
    fn default() -> Self {
        Self {
            status: RuntimeStatus::Stopped,
            peer: None,
            control: None,
            config: AppConfig::default(),
            fault: None,
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
        };
        publish_status(&events, RuntimeStatus::Starting).await;
        let (stop_tx, stop_rx) = watch::channel(false);
        let session_events = events.clone();
        let session_diagnostics = diagnostics.clone();
        let mut task = tokio::spawn(async move {
            match session {
                SessionKind::Host(config) => {
                    run_host(config, stop_rx, session_events, session_diagnostics).await
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
    config: HostConfig,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
    _diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let direction = config
        .direction
        .context("screen direction is required; pass --direction")?;
    platform::validate_capture(&config.devices)?;
    let endpoint = transport::server_endpoint(config.bind, &config.cert, &config.key)?;
    tracing::info!(local = %endpoint.local_addr()?, "host listening");
    publish_status(&events, RuntimeStatus::Listening).await;
    let connection = tokio::select! {
        connection = transport::accept_one(&endpoint) => connection?,
        _ = wait_for_stop(&mut stop) => return Ok(()),
    };
    let remote = connection.remote_address();
    events.send(AppEvent::PeerChanged(Some(remote))).await;
    publish_status(&events, RuntimeStatus::Connected).await;
    tracing::info!(%remote, "client connected");
    run_host_connection(
        connection,
        config.size,
        config.devices,
        direction,
        stop,
        events,
    )
    .await
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
    config: ClientConfig,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let size = match config.size {
        Some(size) => size,
        None => platform::screen_size()?,
    };
    let target = config.target.resolve().await?;
    let bind_ip = if target.is_ipv4() {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        IpAddr::V6(Ipv6Addr::UNSPECIFIED)
    };
    let endpoint = transport::client_endpoint(SocketAddr::new(bind_ip, 0), &config.server_cert)?;
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
            size,
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

async fn run_client_once(
    endpoint: &Endpoint,
    remote: SocketAddr,
    size: ScreenSize,
    mut stop: watch::Receiver<bool>,
    events: AppEventBus,
    diagnostics: Arc<dyn DiagnosticSink>,
) -> Result<()> {
    let connection = tokio::select! {
        connection = transport::connect(endpoint, remote) => connection?,
        _ = wait_for_stop(&mut stop) => return Ok(()),
    };
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
                server_cert: PathBuf::from("server-cert.der"),
                size: Some(ScreenSize::new(100, 100).unwrap()),
                retry_for: Duration::from_secs(30),
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
                size: ScreenSize::new(100, 100).unwrap(),
                devices: Vec::new(),
                direction: None,
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
                size: ScreenSize::new(100, 100).unwrap(),
                devices: vec![PathBuf::from("/dev/null")],
                direction: Some(ScreenDirection::Right),
            }))
            .await
            .unwrap();
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Starting))
        );
        assert_eq!(
            runtime.next_event().await,
            Some(AppEvent::StatusChanged(RuntimeStatus::Listening))
        );
        tokio::time::timeout(Duration::from_secs(2), runtime.shutdown())
            .await
            .expect("active runtime shutdown timed out")
            .unwrap();
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
    fn diagnostics_are_not_required_for_business_behavior() {
        NoopDiagnostics.emit(DiagnosticEvent::MotionDropped { sequence: 4 });
    }
}
