//! Host-side client for the long-lived `alice-computer serve` process.
//!
//! The client owns only process and RPC state.  It never embeds the native
//! runtime, parses diagnostic CLI output, retries actions, or exposes HWNDs.

use alice_computer_use_core::{
    ApplicationCapabilityProfile, CaptureFrameMetadata, ComputerAction, ComputerActionResult,
    ComputerExecutionRequest, ComputerExecutionResult, ComputerObservation, ComputerSessionId,
    ElementId, FrameEncoding, FrameEncodingResult, FrameId, FrameMetadataResult, Screenshot,
    SemanticAction, SemanticActionResult, SemanticObservation, SemanticObservationLimits, Window,
    WindowId,
};
pub use alice_computer_use_sidecar_protocol::HealthResult;
use alice_computer_use_sidecar_protocol::{
    decode_json, error_response, read_frame, transport_error, write_frame, ActionParams,
    CapabilityProbeParams, ExecutionPerformParams, FrameCaptureParams, FrameCaptureResult,
    FrameEncodeParams, FrameEncodeResult, FrameIdParams, HelloResult, RpcError, RpcRequest,
    RpcResponse, ScreenshotParams, SemanticActionParams, SemanticObserveParams,
    SemanticValidateParams, SessionCleanupResult, SessionParams, SessionResult, ShutdownResult,
    BACKEND_NAME, CAPABILITY_CONTRACT_VERSION, PROTOCOL_VERSION,
};
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, ChildStderr, ChildStdout, Command, Stdio},
    sync::{
        atomic::{AtomicU64, Ordering},
        mpsc::{self, Sender},
        Arc, Condvar, Mutex,
    },
    thread,
    time::Duration,
};

#[derive(Debug)]
pub enum SidecarError {
    Rpc(RpcError),
    ProtocolMismatch {
        expected: u32,
        actual: u32,
        backend: String,
    },
    CapabilityContractMismatch {
        expected: u32,
        actual: u32,
    },
    Transport {
        code: String,
        message: String,
        retryable: bool,
        outcome_unknown: bool,
    },
    Timeout {
        method: String,
        retryable: bool,
        outcome_unknown: bool,
    },
    SessionInvalidated {
        session_id: ComputerSessionId,
    },
    SidecarNotRunning,
    InvalidResponse(String),
}

impl SidecarError {
    pub fn code(&self) -> &str {
        match self {
            Self::Rpc(error) => &error.code,
            Self::ProtocolMismatch { .. } => "PROTOCOL_MISMATCH",
            Self::CapabilityContractMismatch { .. } => "CAPABILITY_CONTRACT_MISMATCH",
            Self::Transport { code, .. } => code,
            Self::Timeout { .. } => "TIMEOUT",
            Self::SessionInvalidated { .. } => "INVALID_SESSION",
            Self::SidecarNotRunning => "SIDECAR_NOT_RUNNING",
            Self::InvalidResponse(_) => "INVALID_RESPONSE",
        }
    }

    pub fn outcome_unknown(&self) -> bool {
        match self {
            Self::Rpc(error) => error.outcome_unknown,
            Self::Transport {
                outcome_unknown, ..
            } => *outcome_unknown,
            Self::Timeout {
                outcome_unknown, ..
            } => *outcome_unknown,
            _ => false,
        }
    }

    pub fn retryable(&self) -> bool {
        match self {
            Self::Rpc(error) => error.retryable,
            Self::Transport { retryable, .. } => *retryable,
            Self::Timeout { retryable, .. } => *retryable,
            _ => false,
        }
    }
}

impl std::fmt::Display for SidecarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rpc(error) => write!(f, "{}: {}", error.code, error.message),
            Self::ProtocolMismatch {
                expected,
                actual,
                backend,
            } => write!(
                f,
                "protocol mismatch: expected {expected}, got {actual} ({backend})"
            ),
            Self::CapabilityContractMismatch { expected, actual } => write!(
                f,
                "capability contract mismatch: expected {expected}, got {actual}"
            ),
            Self::Transport { code, message, .. } => write!(f, "{code}: {message}"),
            Self::Timeout { method, .. } => write!(f, "RPC timed out: {method}"),
            Self::SessionInvalidated { session_id } => {
                write!(f, "session invalidated: {session_id}")
            }
            Self::SidecarNotRunning => f.write_str("computer sidecar is not running"),
            Self::InvalidResponse(message) => write!(f, "invalid sidecar response: {message}"),
        }
    }
}

impl std::error::Error for SidecarError {}

struct PendingResponse {
    method: String,
    sender: Sender<RpcResponse>,
}

struct ClientState {
    alive: bool,
    process_id: u32,
    sessions: HashSet<ComputerSessionId>,
}

struct ClientInner {
    writer: Mutex<std::process::ChildStdin>,
    pending: Mutex<HashMap<String, PendingResponse>>,
    state: Mutex<ClientState>,
    state_changed: Condvar,
    next_request_id: AtomicU64,
    default_timeout: Duration,
}

#[derive(Clone)]
pub struct ComputerSidecarClient {
    inner: Arc<ClientInner>,
}

impl ComputerSidecarClient {
    /// Spawn one long-lived `alice-computer serve` process and complete the
    /// protocol handshake.  The caller must explicitly provide the executable
    /// path; this avoids accidentally spawning a per-action diagnostic command.
    pub fn spawn(path: impl AsRef<Path>, timeout: Duration) -> Result<Self, SidecarError> {
        Self::spawn_with_marker(
            path,
            timeout,
            alice_computer_use_core::ALICE_COMPUTER_INPUT_MARKER as usize,
        )
    }

    pub fn spawn_with_marker(
        path: impl AsRef<Path>,
        timeout: Duration,
        input_marker: usize,
    ) -> Result<Self, SidecarError> {
        let mut child = Command::new(path.as_ref())
            .arg("serve")
            .env("ALICE_COMPUTER_INPUT_MARKER", input_marker.to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Self::transport("SPAWN_FAILED", error.to_string(), false, false))?;
        let process_id = child.id();
        let stdin = child.stdin.take().ok_or_else(|| {
            Self::transport("SPAWN_FAILED", "sidecar stdin was not piped", false, false)
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            Self::transport("SPAWN_FAILED", "sidecar stdout was not piped", false, false)
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            Self::transport("SPAWN_FAILED", "sidecar stderr was not piped", false, false)
        })?;
        let inner = Arc::new(ClientInner {
            writer: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            state: Mutex::new(ClientState {
                alive: true,
                process_id,
                sessions: HashSet::new(),
            }),
            state_changed: Condvar::new(),
            next_request_id: AtomicU64::new(1),
            default_timeout: timeout,
        });
        spawn_reader(stdout, Arc::clone(&inner));
        spawn_stderr_reader(stderr, process_id);
        spawn_process_monitor(child, Arc::clone(&inner));
        let client = Self { inner };

        let hello = match client.hello() {
            Ok(hello) => hello,
            Err(error) => {
                let _ = client.shutdown();
                return Err(error);
            }
        };
        if hello.protocol_version != PROTOCOL_VERSION
            || hello.capability_contract_version != CAPABILITY_CONTRACT_VERSION
            || hello.backend != BACKEND_NAME
        {
            let error =
                if hello.protocol_version != PROTOCOL_VERSION || hello.backend != BACKEND_NAME {
                    SidecarError::ProtocolMismatch {
                        expected: PROTOCOL_VERSION,
                        actual: hello.protocol_version,
                        backend: hello.backend,
                    }
                } else {
                    SidecarError::CapabilityContractMismatch {
                        expected: CAPABILITY_CONTRACT_VERSION,
                        actual: hello.capability_contract_version,
                    }
                };
            let _ = client.shutdown();
            return Err(error);
        }
        Ok(client)
    }

    pub fn process_id(&self) -> u32 {
        self.inner
            .state
            .lock()
            .expect("sidecar state poisoned")
            .process_id
    }

    pub fn is_alive(&self) -> bool {
        self.inner
            .state
            .lock()
            .expect("sidecar state poisoned")
            .alive
    }

    pub fn hello(&self) -> Result<HelloResult, SidecarError> {
        self.call_typed("hello", serde_json::json!({}), false)
    }

    pub fn health(&self) -> Result<HealthResult, SidecarError> {
        self.call_typed("health", serde_json::json!({}), false)
    }

    pub fn create_session(&self) -> Result<ComputerSidecarSession, SidecarError> {
        let result: SessionResult =
            self.call_typed("session.create", serde_json::json!({}), false)?;
        self.inner
            .state
            .lock()
            .expect("sidecar state poisoned")
            .sessions
            .insert(result.session_id.clone());
        Ok(ComputerSidecarSession {
            client: self.clone(),
            id: result.session_id,
        })
    }

    pub fn close_session(&self, session_id: &ComputerSessionId) -> Result<(), SidecarError> {
        self.ensure_session(session_id)?;
        let _: Value = self.call_typed(
            "session.close",
            serde_json::to_value(SessionParams {
                session_id: session_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )?;
        self.inner
            .state
            .lock()
            .expect("sidecar state poisoned")
            .sessions
            .remove(session_id);
        Ok(())
    }

    pub fn cleanup_pressed(&self, session_id: &ComputerSessionId) -> Result<(), SidecarError> {
        self.ensure_session(session_id)?;
        let _: SessionCleanupResult = self.call_typed(
            "session.cleanup_pressed",
            serde_json::to_value(SessionParams {
                session_id: session_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            true,
        )?;
        Ok(())
    }

    pub fn window_list(&self, session_id: &ComputerSessionId) -> Result<Vec<Window>, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "window.list",
            serde_json::to_value(SessionParams {
                session_id: session_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn capability_probe(
        &self,
        session_id: &ComputerSessionId,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "capability.probe",
            serde_json::to_value(CapabilityProbeParams {
                session_id: session_id.clone(),
                window_id: window_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn observe(
        &self,
        session_id: &ComputerSessionId,
    ) -> Result<ComputerObservation, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "observe",
            serde_json::to_value(SessionParams {
                session_id: session_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn capture_frame(
        &self,
        session_id: &ComputerSessionId,
        display_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<CaptureFrameMetadata, SidecarError> {
        self.ensure_session(session_id)?;
        let result: FrameCaptureResult = self.call_typed(
            "frame.capture",
            serde_json::to_value(FrameCaptureParams {
                session_id: session_id.clone(),
                display_id,
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )?;
        Ok(result.metadata)
    }

    pub fn frame_metadata(
        &self,
        session_id: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "frame.metadata",
            serde_json::to_value(FrameIdParams {
                session_id: session_id.clone(),
                frame_id: frame_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn encode_frame(
        &self,
        session_id: &ComputerSessionId,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, SidecarError> {
        self.ensure_session(session_id)?;
        let result: FrameEncodeResult = self.call_typed(
            "frame.encode",
            serde_json::to_value(FrameEncodeParams {
                session_id: session_id.clone(),
                frame_id: frame_id.clone(),
                encoding,
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )?;
        Ok(FrameEncodingResult {
            screenshot: result.screenshot,
            encoding: result.encoding,
            cache_hit: result.cache_hit,
            encode_micros: result.encode_micros,
        })
    }

    pub fn release_frame(
        &self,
        session_id: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "frame.release",
            serde_json::to_value(FrameIdParams {
                session_id: session_id.clone(),
                frame_id: frame_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn semantic_observe(
        &self,
        session_id: &ComputerSessionId,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "semantic.observe",
            serde_json::to_value(SemanticObserveParams {
                session_id: session_id.clone(),
                window_id: window_id.clone(),
                max_depth: Some(limits.max_depth),
                max_elements: Some(limits.max_elements),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn validate_element(
        &self,
        session_id: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<(), SidecarError> {
        self.ensure_session(session_id)?;
        let _: Value = self.call_typed(
            "semantic.validate",
            serde_json::to_value(SemanticValidateParams {
                session_id: session_id.clone(),
                element_id: element_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )?;
        Ok(())
    }

    pub fn resolve_element_window(
        &self,
        session_id: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<WindowId, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "semantic.element_window",
            serde_json::to_value(SemanticValidateParams {
                session_id: session_id.clone(),
                element_id: element_id.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn semantic_action(
        &self,
        session_id: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "semantic.action",
            serde_json::to_value(SemanticActionParams {
                session_id: session_id.clone(),
                action: action.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            true,
        )
    }

    pub fn execute_policy(
        &self,
        session_id: &ComputerSessionId,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "execution.perform",
            serde_json::to_value(ExecutionPerformParams {
                session_id: session_id.clone(),
                execution_request: request.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            true,
        )
    }

    pub fn screenshot(
        &self,
        session_id: &ComputerSessionId,
        screen_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<Screenshot, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "screenshot",
            serde_json::to_value(ScreenshotParams {
                session_id: session_id.clone(),
                screen_id,
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            false,
        )
    }

    pub fn action(
        &self,
        session_id: &ComputerSessionId,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, SidecarError> {
        self.ensure_session(session_id)?;
        self.call_typed(
            "action",
            serde_json::to_value(ActionParams {
                session_id: session_id.clone(),
                action: action.clone(),
            })
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))?,
            true,
        )
    }

    /// Graceful, idempotent process shutdown.  No action is retried by this
    /// client.  A later `spawn` creates a new process and therefore new
    /// sessions.
    pub fn shutdown(&self) -> Result<(), SidecarError> {
        if !self.is_alive() {
            return Ok(());
        }
        let _: ShutdownResult = self.call_typed("shutdown", serde_json::json!({}), false)?;
        let state = self.inner.state.lock().expect("sidecar state poisoned");
        let (_state, timeout) = self
            .inner
            .state_changed
            .wait_timeout_while(state, self.inner.default_timeout, |state| state.alive)
            .expect("sidecar state poisoned");
        if timeout.timed_out() {
            return Err(SidecarError::Timeout {
                method: "shutdown".into(),
                retryable: false,
                outcome_unknown: false,
            });
        }
        Ok(())
    }

    /// Forcefully terminates a sidecar that did not honor graceful shutdown.
    /// This is used only by the Host-owned lifecycle boundary for orphan
    /// cleanup; action callers never retry or replay through this path.
    pub fn terminate(&self) -> Result<(), SidecarError> {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
            use windows_sys::Win32::System::Threading::{
                OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE,
            };

            let process_id = self.process_id();
            let was_alive = self.is_alive();
            const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
            let handle =
                unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_ACCESS, 0, process_id) };
            if handle.is_null() {
                if !was_alive {
                    return Ok(());
                }
                return Err(Self::transport(
                    "TERMINATE_FAILED",
                    format!("OpenProcess failed for pid {process_id}"),
                    false,
                    false,
                ));
            }
            let terminated = unsafe { TerminateProcess(handle, 1) } != 0;
            let waited = if terminated {
                unsafe { WaitForSingleObject(handle, 5_000) == WAIT_OBJECT_0 }
            } else {
                false
            };
            unsafe { CloseHandle(handle) };
            if !terminated || !waited {
                return Err(Self::transport(
                    "TERMINATE_FAILED",
                    format!("could not terminate sidecar pid {process_id}"),
                    false,
                    false,
                ));
            }
            mark_dead(&self.inner, "sidecar terminated by host cleanup".into());
            Ok(())
        }

        #[cfg(not(windows))]
        {
            if !self.is_alive() {
                return Ok(());
            }
            Err(Self::transport(
                "TERMINATE_UNSUPPORTED",
                "forced sidecar termination is only implemented on Windows",
                false,
                false,
            ))
        }
    }

    /// The protocol deliberately exposes this classification instead of
    /// silently retrying.  Callers may retry only these observation methods,
    /// and only after an explicit new sidecar/session lifecycle.
    pub fn is_safe_retry_method(method: &str) -> bool {
        matches!(
            method,
            "health"
                | "window.list"
                | "observe"
                | "screenshot"
                | "semantic.observe"
                | "semantic.validate"
        )
    }

    fn ensure_session(&self, session_id: &ComputerSessionId) -> Result<(), SidecarError> {
        let state = self.inner.state.lock().expect("sidecar state poisoned");
        if !state.alive || !state.sessions.contains(session_id) {
            return Err(SidecarError::SessionInvalidated {
                session_id: session_id.clone(),
            });
        }
        Ok(())
    }

    fn call_typed<T: DeserializeOwned>(
        &self,
        method: &str,
        params: Value,
        action: bool,
    ) -> Result<T, SidecarError> {
        let value = self.call_value(method, params, action)?;
        serde_json::from_value(value)
            .map_err(|error| SidecarError::InvalidResponse(error.to_string()))
    }

    fn call_value(&self, method: &str, params: Value, action: bool) -> Result<Value, SidecarError> {
        if !self.is_alive() {
            return Err(if action {
                SidecarError::Transport {
                    code: "OUTCOME_UNKNOWN".into(),
                    message: "sidecar exited while an action may have been in flight".into(),
                    retryable: false,
                    outcome_unknown: true,
                }
            } else {
                SidecarError::SidecarNotRunning
            });
        }
        let request_id = format!(
            "{}-{}",
            std::process::id(),
            self.inner.next_request_id.fetch_add(1, Ordering::Relaxed)
        );
        let request = RpcRequest {
            request_id: request_id.clone(),
            method: method.to_owned(),
            params,
        };
        let (sender, receiver) = mpsc::channel();
        self.inner
            .pending
            .lock()
            .expect("sidecar pending map poisoned")
            .insert(
                request_id.clone(),
                PendingResponse {
                    method: method.to_owned(),
                    sender,
                },
            );
        let write_result = write_frame(
            &mut *self.inner.writer.lock().expect("sidecar writer poisoned"),
            &request,
        );
        if let Err(error) = write_result {
            self.inner
                .pending
                .lock()
                .expect("sidecar pending map poisoned")
                .remove(&request_id);
            mark_dead(&self.inner, format!("request write failed: {error}"));
            return Err(Self::transport(
                if action {
                    "OUTCOME_UNKNOWN"
                } else {
                    "TRANSPORT_ERROR"
                },
                error.to_string(),
                false,
                action,
            ));
        }
        match receiver.recv_timeout(self.inner.default_timeout) {
            Ok(response) => {
                if response.ok {
                    response.result.ok_or_else(|| {
                        SidecarError::InvalidResponse("ok response had no result".into())
                    })
                } else {
                    Err(SidecarError::Rpc(response.error.unwrap_or_else(|| {
                        RpcError::new("MALFORMED_RESPONSE", "error response had no error object")
                    })))
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.inner
                    .pending
                    .lock()
                    .expect("sidecar pending map poisoned")
                    .remove(&request_id);
                // The protocol is intentionally single-process and actions
                // are never replayed. After a deadline we cannot prove where
                // the server is in the request, so quarantine the entire
                // process before returning. This prevents a timed-out action
                // from completing later while the host sends new input into
                // the same desktop session.
                let termination = self.terminate();
                if let Err(error) = termination {
                    mark_dead(
                        &self.inner,
                        format!("sidecar timed out and quarantine failed: {error}"),
                    );
                }
                Err(SidecarError::Timeout {
                    method: method.to_owned(),
                    retryable: Self::is_safe_retry_method(method),
                    outcome_unknown: action,
                })
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(Self::transport(
                "TRANSPORT_ERROR",
                "sidecar response channel closed",
                Self::is_safe_retry_method(method),
                action,
            )),
        }
    }

    fn transport(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        outcome_unknown: bool,
    ) -> SidecarError {
        SidecarError::Transport {
            code: code.into(),
            message: message.into(),
            retryable,
            outcome_unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ComputerHostServiceConfig {
    pub sidecar_path: PathBuf,
    pub request_timeout: Duration,
    pub input_marker: usize,
}

impl ComputerHostServiceConfig {
    pub fn new(sidecar_path: impl Into<PathBuf>, request_timeout: Duration) -> Self {
        Self {
            sidecar_path: sidecar_path.into(),
            request_timeout,
            input_marker: new_input_marker(),
        }
    }
}

fn new_input_marker() -> usize {
    // Keep the attribution marker within a positive 31-bit value. Windows
    // documents dwExtraInfo as ULONG_PTR, but low-level hook/injection chains
    // can cross process bitness boundaries. A pointer-width random nonce can
    // lose its high bits in such a chain and make Alice's own mouse button
    // events look like a different injector.
    let mut bytes = [0u8; std::mem::size_of::<u32>()];
    if getrandom::fill(&mut bytes).is_ok() {
        let marker = (u32::from_ne_bytes(bytes) & 0x3fff_ffff) | 0x4000_0000;
        return marker as usize;
    }
    // This is attribution metadata, not an authorization boundary. Keep a
    // non-zero fallback for environments where the OS random provider is not
    // available, while normal hosts use a per-service nonce.
    (alice_computer_use_core::ALICE_COMPUTER_INPUT_MARKER as u32 & 0x3fff_ffff | 0x4000_0000)
        as usize
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ComputerHostServiceState {
    Stopped,
    Ready,
    Crashed,
    ProtocolMismatch,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComputerHostStatus {
    pub state: ComputerHostServiceState,
    pub process_id: Option<u32>,
    pub sidecar_version: Option<String>,
    pub protocol_version: Option<u32>,
    pub capability_contract_version: Option<u32>,
    pub session_count: usize,
    pub generation: u64,
}

#[derive(Debug)]
pub enum ComputerHostError {
    Sidecar(SidecarError),
    ResourceUnavailable(String),
    AlreadyRunning {
        process_id: u32,
    },
    ProtocolMismatch {
        expected: u32,
        actual: u32,
        backend: String,
    },
    CapabilityContractMismatch {
        expected: u32,
        actual: u32,
    },
}

impl std::fmt::Display for ComputerHostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Sidecar(error) => write!(f, "computer sidecar: {error}"),
            Self::ResourceUnavailable(message) => f.write_str(message),
            Self::AlreadyRunning { process_id } => {
                write!(f, "computer sidecar is already running (pid {process_id})")
            }
            Self::ProtocolMismatch {
                expected,
                actual,
                backend,
            } => write!(
                f,
                "computer sidecar protocol mismatch: expected {expected}, got {actual} ({backend})"
            ),
            Self::CapabilityContractMismatch { expected, actual } => write!(
                f,
                "computer capability contract mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for ComputerHostError {}

impl From<SidecarError> for ComputerHostError {
    fn from(error: SidecarError) -> Self {
        match error {
            SidecarError::ProtocolMismatch {
                expected,
                actual,
                backend,
            } => Self::ProtocolMismatch {
                expected,
                actual,
                backend,
            },
            SidecarError::CapabilityContractMismatch { expected, actual } => {
                Self::CapabilityContractMismatch { expected, actual }
            }
            other => Self::Sidecar(other),
        }
    }
}

struct ComputerHostServiceInner {
    config: ComputerHostServiceConfig,
    client: Option<ComputerSidecarClient>,
    state: ComputerHostServiceState,
    sidecar_version: Option<String>,
    protocol_version: Option<u32>,
    capability_contract_version: Option<u32>,
    sessions: HashSet<ComputerSessionId>,
    generation: u64,
}

/// Host-owned sidecar lifecycle.  This is deliberately narrower than the
/// future Broker: R0 owns process, protocol, and logical-session lifetime only.
/// No production caller receives a client constructed outside this service.
#[derive(Clone)]
pub struct ComputerHostService {
    inner: Arc<Mutex<ComputerHostServiceInner>>,
    lifecycle_gate: Arc<Mutex<()>>,
}

impl ComputerHostService {
    pub fn new(config: ComputerHostServiceConfig) -> Self {
        Self {
            inner: Arc::new(Mutex::new(ComputerHostServiceInner {
                config,
                client: None,
                state: ComputerHostServiceState::Stopped,
                sidecar_version: None,
                protocol_version: None,
                capability_contract_version: None,
                sessions: HashSet::new(),
                generation: 0,
            })),
            lifecycle_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn sidecar_path(&self) -> PathBuf {
        self.inner
            .lock()
            .expect("computer host service poisoned")
            .config
            .sidecar_path
            .clone()
    }

    /// Starts the bundled sidecar only when a Host operation needs it.
    pub fn ensure_started(&self) -> Result<ComputerHostStatus, ComputerHostError> {
        let mut inner = self.inner.lock().expect("computer host service poisoned");
        if inner.state == ComputerHostServiceState::ProtocolMismatch {
            return Err(ComputerHostError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: inner.protocol_version.unwrap_or_default(),
                backend: "unknown".into(),
            });
        }
        if let Some(client) = inner.client.as_ref() {
            if client.is_alive() {
                return Ok(Self::status_locked(&inner));
            }
            inner.client = None;
            inner.sessions.clear();
            inner.state = ComputerHostServiceState::Crashed;
            inner.sidecar_version = None;
            inner.protocol_version = None;
            inner.capability_contract_version = None;
        }

        if !inner.config.sidecar_path.is_file() {
            return Err(ComputerHostError::ResourceUnavailable(format!(
                "bundled computer sidecar is missing: {}",
                inner.config.sidecar_path.display()
            )));
        }

        let client = match ComputerSidecarClient::spawn_with_marker(
            &inner.config.sidecar_path,
            inner.config.request_timeout,
            inner.config.input_marker,
        ) {
            Ok(client) => client,
            Err(error) => {
                inner.state = if matches!(
                    &error,
                    SidecarError::ProtocolMismatch { .. }
                        | SidecarError::CapabilityContractMismatch { .. }
                ) {
                    ComputerHostServiceState::ProtocolMismatch
                } else {
                    ComputerHostServiceState::Crashed
                };
                return Err(Self::host_error(error));
            }
        };
        let hello = match client.hello() {
            Ok(hello) => hello,
            Err(error) => {
                let _ = client.shutdown();
                inner.state = if matches!(
                    &error,
                    SidecarError::ProtocolMismatch { .. }
                        | SidecarError::CapabilityContractMismatch { .. }
                ) {
                    ComputerHostServiceState::ProtocolMismatch
                } else {
                    ComputerHostServiceState::Crashed
                };
                return Err(Self::host_error(error));
            }
        };
        let health = match client.health() {
            Ok(health) => health,
            Err(error) => {
                let _ = client.shutdown();
                inner.state = ComputerHostServiceState::Crashed;
                return Err(error.into());
            }
        };
        if !health.ready || !health.initialized || health.pid != client.process_id() {
            let _ = client.shutdown();
            inner.state = ComputerHostServiceState::Crashed;
            return Err(ComputerHostError::Sidecar(SidecarError::InvalidResponse(
                "sidecar handshake completed without a ready initialized process".into(),
            )));
        }

        inner.client = Some(client);
        inner.state = ComputerHostServiceState::Ready;
        inner.sidecar_version = Some(hello.sidecar_version);
        inner.protocol_version = Some(hello.protocol_version);
        inner.capability_contract_version = Some(hello.capability_contract_version);
        inner.generation = inner.generation.saturating_add(1);
        Ok(Self::status_locked(&inner))
    }

    pub fn health(&self) -> Result<HealthResult, ComputerHostError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect("computer host lifecycle poisoned");
        self.ensure_started()?;
        let client = self.client()?;
        client.health().map_err(Into::into)
    }

    pub fn input_marker(&self) -> usize {
        self.inner
            .lock()
            .expect("computer host service poisoned")
            .config
            .input_marker
    }

    pub fn create_session(&self) -> Result<ComputerSessionId, ComputerHostError> {
        Ok(self.open_session()?.id().clone())
    }

    /// Opens a session through the Host boundary. The underlying sidecar
    /// client is intentionally private to the returned Host session.
    pub fn open_session(&self) -> Result<ComputerHostSession, ComputerHostError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect("computer host lifecycle poisoned");
        self.ensure_started()?;
        let client = self.client()?;
        let session = client.create_session()?;
        let id = session.id().clone();
        self.inner
            .lock()
            .expect("computer host service poisoned")
            .sessions
            .insert(id.clone());
        Ok(ComputerHostSession {
            service: self.clone(),
            client,
            id,
        })
    }

    pub fn close_session(&self, session_id: &ComputerSessionId) -> Result<(), ComputerHostError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect("computer host lifecycle poisoned");
        let client = self.client()?;
        let result = client
            .close_session(session_id)
            .map_err(ComputerHostError::from);
        let mut inner = self.inner.lock().expect("computer host service poisoned");
        inner.sessions.remove(session_id);
        if !client.is_alive() {
            inner.client = None;
            inner.state = ComputerHostServiceState::Crashed;
            inner.sessions.clear();
        }
        result
    }

    pub fn shutdown(&self) -> Result<(), ComputerHostError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect("computer host lifecycle poisoned");
        let client = {
            let mut inner = self.inner.lock().expect("computer host service poisoned");
            inner.sessions.clear();
            inner.client.take()
        };
        let Some(client) = client else {
            self.inner
                .lock()
                .expect("computer host service poisoned")
                .state = ComputerHostServiceState::Stopped;
            return Ok(());
        };

        let result = client.shutdown();
        if let Err(error) = result {
            let _ = client.terminate();
            let mut inner = self.inner.lock().expect("computer host service poisoned");
            inner.state = ComputerHostServiceState::Crashed;
            return Err(error.into());
        }
        self.inner
            .lock()
            .expect("computer host service poisoned")
            .state = ComputerHostServiceState::Stopped;
        Ok(())
    }

    pub fn status(&self) -> ComputerHostStatus {
        let mut inner = self.inner.lock().expect("computer host service poisoned");
        if inner
            .client
            .as_ref()
            .is_some_and(|client| !client.is_alive())
        {
            inner.client = None;
            inner.sessions.clear();
            inner.state = ComputerHostServiceState::Crashed;
            inner.sidecar_version = None;
            inner.protocol_version = None;
            inner.capability_contract_version = None;
        }
        Self::status_locked(&inner)
    }

    /// R0 crash recovery: a dead process may be started again, but no old
    /// logical session is reused.
    pub fn restart_after_crash(&self) -> Result<ComputerHostStatus, ComputerHostError> {
        let _lifecycle = self
            .lifecycle_gate
            .lock()
            .expect("computer host lifecycle poisoned");
        let status = self.status();
        if matches!(status.state, ComputerHostServiceState::ProtocolMismatch) {
            return Err(ComputerHostError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                actual: status.protocol_version.unwrap_or_default(),
                backend: "unknown".into(),
            });
        }
        if matches!(status.state, ComputerHostServiceState::Ready) {
            return Err(ComputerHostError::AlreadyRunning {
                process_id: status.process_id.unwrap_or_default(),
            });
        }
        self.ensure_started()
    }

    fn client(&self) -> Result<ComputerSidecarClient, ComputerHostError> {
        self.inner
            .lock()
            .expect("computer host service poisoned")
            .client
            .clone()
            .ok_or(ComputerHostError::Sidecar(SidecarError::SidecarNotRunning))
    }

    fn host_error(error: SidecarError) -> ComputerHostError {
        match error {
            SidecarError::ProtocolMismatch {
                expected,
                actual,
                backend,
            } => ComputerHostError::ProtocolMismatch {
                expected,
                actual,
                backend,
            },
            SidecarError::CapabilityContractMismatch { expected, actual } => {
                ComputerHostError::CapabilityContractMismatch { expected, actual }
            }
            error => ComputerHostError::Sidecar(error),
        }
    }

    fn status_locked(inner: &ComputerHostServiceInner) -> ComputerHostStatus {
        ComputerHostStatus {
            state: inner.state,
            process_id: inner.client.as_ref().map(ComputerSidecarClient::process_id),
            sidecar_version: inner.sidecar_version.clone(),
            protocol_version: inner.protocol_version,
            capability_contract_version: inner.capability_contract_version,
            session_count: inner.sessions.len(),
            generation: inner.generation,
        }
    }
}

impl Drop for ComputerHostService {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 {
            let _ = self.shutdown();
        }
    }
}

/// A logical session issued by the Host service. It provides the operation
/// surface required by the Broker without exposing a spawnable sidecar client
/// to production callers.
#[derive(Clone)]
pub struct ComputerHostSession {
    service: ComputerHostService,
    client: ComputerSidecarClient,
    id: ComputerSessionId,
}

impl ComputerHostSession {
    pub fn id(&self) -> &ComputerSessionId {
        &self.id
    }

    pub fn window_list(&self) -> Result<Vec<Window>, ComputerHostError> {
        self.client.window_list(&self.id).map_err(Into::into)
    }

    pub fn observe(&self) -> Result<ComputerObservation, ComputerHostError> {
        self.client.observe(&self.id).map_err(Into::into)
    }

    pub fn capability_probe(
        &self,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerHostError> {
        self.client
            .capability_probe(&self.id, window_id)
            .map_err(Into::into)
    }

    pub fn semantic_observe(
        &self,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerHostError> {
        self.client
            .semantic_observe(&self.id, window_id, limits)
            .map_err(Into::into)
    }

    pub fn validate_element(&self, element_id: &ElementId) -> Result<(), ComputerHostError> {
        self.client
            .validate_element(&self.id, element_id)
            .map_err(Into::into)
    }

    pub fn resolve_element_window(
        &self,
        element_id: &ElementId,
    ) -> Result<WindowId, ComputerHostError> {
        self.client
            .resolve_element_window(&self.id, element_id)
            .map_err(Into::into)
    }

    pub fn capture_frame(
        &self,
        display_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<CaptureFrameMetadata, ComputerHostError> {
        self.client
            .capture_frame(&self.id, display_id)
            .map_err(Into::into)
    }

    pub fn frame_metadata(
        &self,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, ComputerHostError> {
        self.client
            .frame_metadata(&self.id, frame_id)
            .map_err(Into::into)
    }

    pub fn encode_frame(
        &self,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, ComputerHostError> {
        self.client
            .encode_frame(&self.id, frame_id, encoding)
            .map_err(Into::into)
    }

    pub fn release_frame(
        &self,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, ComputerHostError> {
        self.client
            .release_frame(&self.id, frame_id)
            .map_err(Into::into)
    }

    pub fn screenshot(
        &self,
        screen_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<Screenshot, ComputerHostError> {
        self.client
            .screenshot(&self.id, screen_id)
            .map_err(Into::into)
    }

    pub fn execute_policy(
        &self,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, ComputerHostError> {
        self.client
            .execute_policy(&self.id, request)
            .map_err(Into::into)
    }

    pub fn semantic_action(
        &self,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerHostError> {
        self.client
            .semantic_action(&self.id, action)
            .map_err(Into::into)
    }

    pub fn close(&self) -> Result<(), ComputerHostError> {
        self.service.close_session(&self.id)
    }

    pub fn cleanup_pressed(&self) -> Result<(), ComputerHostError> {
        self.client.cleanup_pressed(&self.id).map_err(Into::into)
    }
}

impl Drop for ComputerSidecarClient {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 1 && self.is_alive() {
            let _ = self.shutdown();
        }
    }
}

#[derive(Clone)]
pub struct ComputerSidecarSession {
    client: ComputerSidecarClient,
    id: ComputerSessionId,
}

impl ComputerSidecarSession {
    pub fn id(&self) -> &ComputerSessionId {
        &self.id
    }

    pub fn window_list(&self) -> Result<Vec<Window>, SidecarError> {
        self.client.window_list(&self.id)
    }

    pub fn capability_probe(
        &self,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, SidecarError> {
        self.client.capability_probe(&self.id, window_id)
    }

    pub fn observe(&self) -> Result<ComputerObservation, SidecarError> {
        self.client.observe(&self.id)
    }

    pub fn capture_frame(
        &self,
        display_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<CaptureFrameMetadata, SidecarError> {
        self.client.capture_frame(&self.id, display_id)
    }

    pub fn frame_metadata(&self, frame_id: &FrameId) -> Result<FrameMetadataResult, SidecarError> {
        self.client.frame_metadata(&self.id, frame_id)
    }

    pub fn encode_frame(
        &self,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, SidecarError> {
        self.client.encode_frame(&self.id, frame_id, encoding)
    }

    pub fn release_frame(&self, frame_id: &FrameId) -> Result<FrameMetadataResult, SidecarError> {
        self.client.release_frame(&self.id, frame_id)
    }

    pub fn semantic_observe(
        &self,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, SidecarError> {
        self.client.semantic_observe(&self.id, window_id, limits)
    }

    pub fn validate_element(&self, element_id: &ElementId) -> Result<(), SidecarError> {
        self.client.validate_element(&self.id, element_id)
    }

    pub fn resolve_element_window(&self, element_id: &ElementId) -> Result<WindowId, SidecarError> {
        self.client.resolve_element_window(&self.id, element_id)
    }

    pub fn semantic_action(
        &self,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, SidecarError> {
        self.client.semantic_action(&self.id, action)
    }

    pub fn execute_policy(
        &self,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, SidecarError> {
        self.client.execute_policy(&self.id, request)
    }

    pub fn screenshot(
        &self,
        screen_id: Option<alice_computer_use_core::ScreenId>,
    ) -> Result<Screenshot, SidecarError> {
        self.client.screenshot(&self.id, screen_id)
    }

    pub fn action(&self, action: &ComputerAction) -> Result<ComputerActionResult, SidecarError> {
        self.client.action(&self.id, action)
    }

    pub fn close(&self) -> Result<(), SidecarError> {
        self.client.close_session(&self.id)
    }

    pub fn cleanup_pressed(&self) -> Result<(), SidecarError> {
        self.client.cleanup_pressed(&self.id)
    }
}

fn spawn_reader(stdout: ChildStdout, inner: Arc<ClientInner>) {
    thread::Builder::new()
        .name("alice-computer-rpc-reader".into())
        .spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                match read_frame(&mut reader) {
                    Ok(Some(payload)) => match decode_json::<RpcResponse>(&payload) {
                        Ok(response) => dispatch_response(&inner, response),
                        Err(error) => {
                            mark_dead(&inner, format!("invalid sidecar response: {error}"));
                            break;
                        }
                    },
                    Ok(None) => {
                        mark_dead(&inner, "sidecar stdout reached EOF".into());
                        break;
                    }
                    Err(error) => {
                        mark_dead(&inner, format!("sidecar frame read failed: {error}"));
                        break;
                    }
                }
            }
        })
        .expect("failed to start sidecar RPC reader");
}

fn spawn_stderr_reader(stderr: ChildStderr, process_id: u32) {
    thread::Builder::new()
        .name("alice-computer-stderr".into())
        .spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("[alice-computer:{process_id}] {line}");
            }
        })
        .expect("failed to start sidecar stderr reader");
}

fn spawn_process_monitor(mut child: Child, inner: Arc<ClientInner>) {
    thread::Builder::new()
        .name("alice-computer-process-monitor".into())
        .spawn(move || {
            let status = child.wait();
            let reason = match status {
                Ok(status) => format!("sidecar exited with {status}"),
                Err(error) => format!("sidecar wait failed: {error}"),
            };
            mark_dead(&inner, reason);
        })
        .expect("failed to start sidecar process monitor");
}

fn dispatch_response(inner: &Arc<ClientInner>, response: RpcResponse) {
    let pending = inner
        .pending
        .lock()
        .expect("sidecar pending map poisoned")
        .remove(&response.request_id);
    if let Some(pending) = pending {
        let _ = pending.sender.send(response);
    }
}

fn mark_dead(inner: &Arc<ClientInner>, reason: String) {
    let should_notify = {
        let mut state = inner.state.lock().expect("sidecar state poisoned");
        if !state.alive {
            false
        } else {
            state.alive = false;
            state.sessions.clear();
            true
        }
    };
    if !should_notify {
        return;
    }
    let pending = std::mem::take(&mut *inner.pending.lock().expect("sidecar pending map poisoned"));
    for (request_id, pending) in pending {
        let unknown = !ComputerSidecarClient::is_safe_retry_method(&pending.method);
        let mut error = transport_error(reason.clone(), unknown);
        if unknown {
            error.code = "OUTCOME_UNKNOWN".into();
        }
        let response = error_response(request_id, error);
        let _ = pending.sender.send(response);
    }
    inner.state_changed.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_observation_methods_are_safe_retry_candidates() {
        assert!(ComputerSidecarClient::is_safe_retry_method("health"));
        assert!(ComputerSidecarClient::is_safe_retry_method("screenshot"));
        assert!(!ComputerSidecarClient::is_safe_retry_method("action"));
        assert!(!ComputerSidecarClient::is_safe_retry_method(
            "semantic.action"
        ));
        assert!(!ComputerSidecarClient::is_safe_retry_method(
            "session.create"
        ));
    }

    #[test]
    fn input_marker_survives_cross_bitness_extra_info_paths() {
        for _ in 0..32 {
            let marker = new_input_marker();
            assert_ne!(marker, 0);
            assert!(u32::try_from(marker).is_ok());
            assert_eq!(marker, marker as u32 as usize);
        }
    }
}
