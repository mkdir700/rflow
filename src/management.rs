use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    pairing::PairingRequestId,
    runtime::{AppCommand, ApplicationHandle},
};

#[cfg(unix)]
use crate::identity::application_config_directory;

const MANAGEMENT_PROTOCOL_VERSION: u16 = 1;
const MAX_MANAGEMENT_FRAME: usize = 64 * 1024;
#[cfg(unix)]
pub const MANAGEMENT_SOCKET_FILE: &str = "management.sock";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagementRequest {
    Pending,
    Accept(u64),
    Reject(u64),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingPairing {
    pub request_id: String,
    pub device_name: String,
    pub address: String,
    pub fingerprint: String,
    pub code: String,
    pub expires_in_seconds: u64,
    pub expires_at_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ManagementResponse {
    Pending(Option<PendingPairing>),
    DecisionQueued,
    Error(String),
}

#[derive(Debug, Serialize, Deserialize)]
struct Envelope<T> {
    version: u16,
    payload: T,
}

#[cfg(unix)]
pub fn default_endpoint_path() -> Result<PathBuf> {
    Ok(application_config_directory()?.join(MANAGEMENT_SOCKET_FILE))
}

#[cfg(windows)]
pub fn default_endpoint_path() -> Result<PathBuf> {
    Ok(PathBuf::from(format!(
        r"\\.\pipe\rflow-management-{}",
        platform::current_user_sid_string()?
    )))
}

async fn dispatch(
    request: ManagementRequest,
    application: &ApplicationHandle,
) -> ManagementResponse {
    let pending = application.snapshot().pairing;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    match request {
        ManagementRequest::Pending => {
            ManagementResponse::Pending(pending.map(|request| PendingPairing {
                request_id: request.request_id.to_string(),
                device_name: request.device_name,
                address: request.address.to_string(),
                fingerprint: request.fingerprint.to_string(),
                code: request.code.to_string(),
                expires_in_seconds: request.expires_at_unix_seconds.saturating_sub(now),
                expires_at_unix_seconds: request.expires_at_unix_seconds,
            }))
        }
        ManagementRequest::Accept(value) | ManagementRequest::Reject(value) => {
            let request_id = PairingRequestId(value);
            if !pending
                .as_ref()
                .is_some_and(|request| request.request_id == request_id)
            {
                return ManagementResponse::Error(format!(
                    "pairing request {request_id} is not pending"
                ));
            }
            let command = if matches!(request, ManagementRequest::Accept(_)) {
                AppCommand::AcceptPairing(request_id)
            } else {
                AppCommand::RejectPairing(request_id)
            };
            match application.send(command).await {
                Ok(()) => ManagementResponse::DecisionQueued,
                Err(error) => ManagementResponse::Error(error.to_string()),
            }
        }
    }
}

#[cfg(unix)]
mod platform {
    use std::{fs, os::unix::fs::PermissionsExt};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{UnixListener, UnixStream},
        sync::watch,
    };

    use super::*;

    #[derive(Debug)]
    pub struct ManagementServer {
        shutdown: watch::Sender<bool>,
        task: Option<tokio::task::JoinHandle<Result<()>>>,
        path: PathBuf,
    }

    impl ManagementServer {
        pub async fn bind(path: impl AsRef<Path>, application: ApplicationHandle) -> Result<Self> {
            let path = path.as_ref().to_owned();
            prepare_endpoint(&path).await?;
            let listener = UnixListener::bind(&path).context("bind local management socket")?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
                .context("secure local management socket")?;
            let (shutdown, mut shutdown_rx) = watch::channel(false);
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        accepted = listener.accept() => {
                            let (stream, _) = accepted.context("accept local management client")?;
                            let application = application.clone();
                            tokio::spawn(async move {
                                if let Err(error) = handle_client(stream, application).await {
                                    tracing::warn!(%error, "local management request failed");
                                }
                            });
                        }
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                return Ok(());
                            }
                        }
                    }
                }
            });
            Ok(Self {
                shutdown,
                task: Some(task),
                path,
            })
        }

        pub async fn shutdown(mut self) -> Result<()> {
            let _ = self.shutdown.send(true);
            if let Some(task) = self.task.take() {
                task.await.context("join management server")??;
            }
            remove_if_present(&self.path)?;
            Ok(())
        }
    }

    impl Drop for ManagementServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
            let _ = remove_if_present(&self.path);
        }
    }

    async fn prepare_endpoint(path: &Path) -> Result<()> {
        let directory = path
            .parent()
            .context("management socket has no directory")?;
        fs::create_dir_all(directory).context("create management directory")?;
        fs::set_permissions(directory, fs::Permissions::from_mode(0o700))
            .context("secure management directory")?;
        if !path.exists() {
            return Ok(());
        }
        if UnixStream::connect(path).await.is_ok() {
            bail!("another rflow host is already using {}", path.display());
        }
        remove_if_present(path)
    }

    async fn handle_client(mut stream: UnixStream, application: ApplicationHandle) -> Result<()> {
        let request: Envelope<ManagementRequest> = read_frame(&mut stream).await?;
        let response = if request.version != MANAGEMENT_PROTOCOL_VERSION {
            ManagementResponse::Error(format!(
                "unsupported management protocol version {}",
                request.version
            ))
        } else {
            dispatch(request.payload, &application).await
        };
        write_frame(
            &mut stream,
            &Envelope {
                version: MANAGEMENT_PROTOCOL_VERSION,
                payload: response,
            },
        )
        .await
    }

    pub async fn request(
        path: impl AsRef<Path>,
        request: ManagementRequest,
    ) -> Result<ManagementResponse> {
        let mut stream = UnixStream::connect(path.as_ref())
            .await
            .with_context(|| format!("connect to rflow host at {}", path.as_ref().display()))?;
        write_frame(
            &mut stream,
            &Envelope {
                version: MANAGEMENT_PROTOCOL_VERSION,
                payload: request,
            },
        )
        .await?;
        let response: Envelope<ManagementResponse> = read_frame(&mut stream).await?;
        if response.version != MANAGEMENT_PROTOCOL_VERSION {
            bail!(
                "unsupported management protocol version {}",
                response.version
            );
        }
        Ok(response.payload)
    }

    async fn read_frame<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> Result<T> {
        let length = stream
            .read_u32()
            .await
            .context("read management frame length")? as usize;
        if length > MAX_MANAGEMENT_FRAME {
            bail!("management frame exceeds {MAX_MANAGEMENT_FRAME} bytes");
        }
        let mut bytes = vec![0_u8; length];
        stream
            .read_exact(&mut bytes)
            .await
            .context("read management frame")?;
        postcard::from_bytes(&bytes).context("decode management frame")
    }

    async fn write_frame<T: Serialize>(stream: &mut UnixStream, value: &T) -> Result<()> {
        let bytes = postcard::to_allocvec(value).context("encode management frame")?;
        if bytes.len() > MAX_MANAGEMENT_FRAME {
            bail!("management frame exceeds {MAX_MANAGEMENT_FRAME} bytes");
        }
        stream
            .write_u32(u32::try_from(bytes.len())?)
            .await
            .context("write management frame length")?;
        stream
            .write_all(&bytes)
            .await
            .context("write management frame")
    }

    fn remove_if_present(path: &Path) -> Result<()> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error).context("remove local management socket"),
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::Arc;

        use crate::{
            pairing::{PairingCode, PairingRequestId},
            runtime::{
                AppCommand, NoopDiagnostics, PairingRequestSummary, RuntimeHandle, RuntimeSnapshot,
                application_test_channel,
            },
            trust::DeviceId,
        };

        use super::*;

        #[tokio::test]
        async fn owner_only_channel_reads_snapshot_and_rejects_unknown_decisions() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("management.sock");
            let runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
            let server = ManagementServer::bind(&path, runtime.application_handle())
                .await
                .unwrap();

            assert_eq!(
                request(&path, ManagementRequest::Pending).await.unwrap(),
                ManagementResponse::Pending(None)
            );
            assert!(matches!(
                request(&path, ManagementRequest::Accept(42)).await.unwrap(),
                ManagementResponse::Error(message) if message.contains("not pending")
            ));
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);

            server.shutdown().await.unwrap();
            runtime.shutdown().await.unwrap();
            assert!(!path.exists());
        }

        #[tokio::test]
        async fn a_live_endpoint_enforces_single_instance() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("management.sock");
            let first_runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
            let first = ManagementServer::bind(&path, first_runtime.application_handle())
                .await
                .unwrap();
            let second_runtime = RuntimeHandle::spawn(Arc::new(NoopDiagnostics)).unwrap();
            let error = ManagementServer::bind(&path, second_runtime.application_handle())
                .await
                .unwrap_err();
            assert!(error.to_string().contains("another rflow host"));
            first.shutdown().await.unwrap();
            first_runtime.shutdown().await.unwrap();
            second_runtime.shutdown().await.unwrap();
        }

        #[tokio::test]
        async fn pending_request_is_rendered_and_the_matching_decision_uses_app_command() {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("management.sock");
            let request_id = PairingRequestId(42);
            let (application, mut commands) = application_test_channel(RuntimeSnapshot {
                pairing: Some(PairingRequestSummary {
                    request_id,
                    device_name: "macmini".to_owned(),
                    address: "192.168.1.82:24801".parse().unwrap(),
                    fingerprint: DeviceId([7; 32]),
                    code: PairingCode::from_value(482_731).unwrap(),
                    expires_at_unix_seconds: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_secs()
                        + 120,
                }),
                ..RuntimeSnapshot::default()
            });
            let server = ManagementServer::bind(&path, application).await.unwrap();

            let ManagementResponse::Pending(Some(pending)) =
                request(&path, ManagementRequest::Pending).await.unwrap()
            else {
                panic!("expected pending request")
            };
            assert_eq!(pending.request_id, "p-000000000000002a");
            assert_eq!(pending.code, "482 731");
            assert_eq!(
                request(&path, ManagementRequest::Accept(42)).await.unwrap(),
                ManagementResponse::DecisionQueued
            );
            assert!(matches!(
                commands.recv().await,
                Some(AppCommand::AcceptPairing(id)) if id == request_id
            ));

            server.shutdown().await.unwrap();
        }
    }
}

#[cfg(unix)]
pub use platform::{ManagementServer, request};

#[cfg(windows)]
mod platform {
    use std::{ffi::c_void, mem::size_of, ptr};

    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
        sync::watch,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_INSUFFICIENT_BUFFER, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    use super::*;

    #[derive(Debug)]
    pub struct ManagementServer {
        shutdown: watch::Sender<bool>,
        task: Option<tokio::task::JoinHandle<Result<()>>>,
    }

    impl ManagementServer {
        pub async fn bind(path: impl AsRef<Path>, application: ApplicationHandle) -> Result<Self> {
            let path = path.as_ref().to_owned();
            let mut server = create_server(&path, true).context("bind local management pipe")?;
            let (shutdown, mut shutdown_rx) = watch::channel(false);
            let task = tokio::spawn(async move {
                loop {
                    tokio::select! {
                        connected = server.connect() => connected.context("accept local management client")?,
                        changed = shutdown_rx.changed() => {
                            if changed.is_err() || *shutdown_rx.borrow() {
                                return Ok(());
                            }
                            continue;
                        }
                    }
                    let connected = server;
                    server = create_server(&path, false).context("create next management pipe")?;
                    let application = application.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_client(connected, application).await {
                            tracing::warn!(%error, "local management request failed");
                        }
                    });
                }
            });
            Ok(Self {
                shutdown,
                task: Some(task),
            })
        }

        pub async fn shutdown(mut self) -> Result<()> {
            let _ = self.shutdown.send(true);
            if let Some(task) = self.task.take() {
                task.await.context("join management server")??;
            }
            Ok(())
        }
    }

    impl Drop for ManagementServer {
        fn drop(&mut self) {
            let _ = self.shutdown.send(true);
        }
    }

    fn create_server(path: &Path, first: bool) -> Result<NamedPipeServer> {
        let mut security = OwnerSecurity::new()?;
        let mut options = ServerOptions::new();
        options
            .first_pipe_instance(first)
            .reject_remote_clients(true);
        // SAFETY: `security.attributes` and its descriptor remain valid for the
        // duration of CreateNamedPipeW. Windows copies the descriptor when the
        // pipe instance is created.
        unsafe {
            options
                .create_with_security_attributes_raw(
                    path,
                    (&mut security.attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
                .context("create owner-only named pipe")
        }
    }

    async fn handle_client(
        mut stream: NamedPipeServer,
        application: ApplicationHandle,
    ) -> Result<()> {
        let request: Envelope<ManagementRequest> = read_frame_server(&mut stream).await?;
        let response = if request.version != MANAGEMENT_PROTOCOL_VERSION {
            ManagementResponse::Error(format!(
                "unsupported management protocol version {}",
                request.version
            ))
        } else {
            dispatch(request.payload, &application).await
        };
        write_frame_server(
            &mut stream,
            &Envelope {
                version: MANAGEMENT_PROTOCOL_VERSION,
                payload: response,
            },
        )
        .await
    }

    pub async fn request(
        path: impl AsRef<Path>,
        request: ManagementRequest,
    ) -> Result<ManagementResponse> {
        let mut stream = ClientOptions::new()
            .open(path.as_ref())
            .with_context(|| format!("connect to rflow host at {}", path.as_ref().display()))?;
        write_frame_client(
            &mut stream,
            &Envelope {
                version: MANAGEMENT_PROTOCOL_VERSION,
                payload: request,
            },
        )
        .await?;
        let response: Envelope<ManagementResponse> = read_frame_client(&mut stream).await?;
        if response.version != MANAGEMENT_PROTOCOL_VERSION {
            bail!(
                "unsupported management protocol version {}",
                response.version
            );
        }
        Ok(response.payload)
    }

    macro_rules! frame_io {
        ($read:ident, $write:ident, $stream:ty) => {
            async fn $read<T: for<'de> Deserialize<'de>>(stream: &mut $stream) -> Result<T> {
                let length = stream
                    .read_u32()
                    .await
                    .context("read management frame length")? as usize;
                if length > MAX_MANAGEMENT_FRAME {
                    bail!("management frame exceeds {MAX_MANAGEMENT_FRAME} bytes");
                }
                let mut bytes = vec![0_u8; length];
                stream
                    .read_exact(&mut bytes)
                    .await
                    .context("read management frame")?;
                postcard::from_bytes(&bytes).context("decode management frame")
            }

            async fn $write<T: Serialize>(stream: &mut $stream, value: &T) -> Result<()> {
                let bytes = postcard::to_allocvec(value).context("encode management frame")?;
                if bytes.len() > MAX_MANAGEMENT_FRAME {
                    bail!("management frame exceeds {MAX_MANAGEMENT_FRAME} bytes");
                }
                stream
                    .write_u32(u32::try_from(bytes.len())?)
                    .await
                    .context("write management frame length")?;
                stream
                    .write_all(&bytes)
                    .await
                    .context("write management frame")
            }
        };
    }

    frame_io!(read_frame_server, write_frame_server, NamedPipeServer);
    frame_io!(read_frame_client, write_frame_client, NamedPipeClient);

    struct OwnerSecurity {
        attributes: SECURITY_ATTRIBUTES,
        descriptor: *mut c_void,
    }

    impl OwnerSecurity {
        fn new() -> Result<Self> {
            let sid = current_user_sid_string()?;
            let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})\0").encode_utf16().collect();
            let mut descriptor = ptr::null_mut();
            let converted = unsafe {
                ConvertStringSecurityDescriptorToSecurityDescriptorW(
                    sddl.as_ptr(),
                    SDDL_REVISION_1,
                    &mut descriptor,
                    ptr::null_mut(),
                )
            };
            if converted == 0 {
                return Err(std::io::Error::last_os_error())
                    .context("create owner-only pipe security descriptor");
            }
            Ok(Self {
                attributes: SECURITY_ATTRIBUTES {
                    nLength: u32::try_from(size_of::<SECURITY_ATTRIBUTES>())?,
                    lpSecurityDescriptor: descriptor,
                    bInheritHandle: 0,
                },
                descriptor,
            })
        }
    }

    impl Drop for OwnerSecurity {
        fn drop(&mut self) {
            if !self.descriptor.is_null() {
                unsafe {
                    LocalFree(self.descriptor);
                }
            }
        }
    }

    pub(super) fn current_user_sid_string() -> Result<String> {
        let mut token = ptr::null_mut();
        if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
            return Err(std::io::Error::last_os_error()).context("open current process token");
        }
        let result = (|| {
            let mut length = 0_u32;
            unsafe {
                GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut length);
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) || length == 0 {
                return Err(error).context("size current user token information");
            }
            let mut buffer = vec![0_u8; length as usize];
            if unsafe {
                GetTokenInformation(
                    token,
                    TokenUser,
                    buffer.as_mut_ptr().cast(),
                    length,
                    &mut length,
                )
            } == 0
            {
                return Err(std::io::Error::last_os_error())
                    .context("read current user token information");
            }
            let user = unsafe { &*(buffer.as_ptr().cast::<TOKEN_USER>()) };
            let mut encoded = ptr::null_mut();
            if unsafe { ConvertSidToStringSidW(user.User.Sid, &mut encoded) } == 0 {
                return Err(std::io::Error::last_os_error()).context("format current user SID");
            }
            let mut units = 0_usize;
            while unsafe { *encoded.add(units) } != 0 {
                units += 1;
            }
            let value = String::from_utf16(unsafe { std::slice::from_raw_parts(encoded, units) })
                .context("decode current user SID")?;
            unsafe {
                LocalFree(encoded.cast());
            }
            Ok(value)
        })();
        unsafe {
            CloseHandle(token);
        }
        result
    }
}

#[cfg(windows)]
pub use platform::{ManagementServer, request};
