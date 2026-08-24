//! The private, length-prefixed JSON protocol used by the Computer Use
//! sidecar and its Alice host client.
//!
//! This crate contains transport/schema code only.  It does not know about
//! Tauri, processes, Win32 handles, UIA, or model/agent behavior.

use alice_computer_use_core::{
    ApplicationCapabilityProfile, CaptureFrameMetadata, ComputerAction, ComputerError,
    ComputerExecutionRequest, ComputerExecutionResult, ComputerSessionId, DesktopSecurityContext,
    ElementId, FrameEncoding, FrameId, ProcessSecurityContext, ScreenId, Screenshot,
    SemanticAction, SemanticActionResult, SemanticObservationLimits, WindowId,
};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    fmt,
    io::{self, Read, Write},
};

pub const PROTOCOL_VERSION: u32 = 1;
pub const CAPABILITY_CONTRACT_VERSION: u32 = 1;
pub const BACKEND_NAME: &str = "win_native";
pub const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcRequest {
    pub request_id: String,
    pub method: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcResponse {
    pub request_id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RpcError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub outcome_unknown: bool,
}

impl RpcError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retryable: false,
            outcome_unknown: false,
        }
    }

    pub fn retryable(mut self, value: bool) -> Self {
        self.retryable = value;
        self
    }

    pub fn unknown_outcome(mut self, value: bool) -> Self {
        self.outcome_unknown = value;
        self
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HelloResult {
    pub protocol_version: u32,
    #[serde(default)]
    pub capability_contract_version: u32,
    pub sidecar_version: String,
    pub backend: String,
    pub capabilities: Vec<CapabilityInfo>,
    pub pid: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityInfo {
    pub name: String,
    pub state: String,
    pub detail: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HealthResult {
    pub ready: bool,
    pub initialized: bool,
    pub session_count: usize,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<ProcessSecurityContext>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desktop: Option<DesktopSecurityContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionResult {
    pub session_id: ComputerSessionId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionParams {
    pub session_id: ComputerSessionId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SessionCleanupResult {
    pub cleaned: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ScreenshotParams {
    pub session_id: ComputerSessionId,
    pub screen_id: Option<ScreenId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrameCaptureParams {
    pub session_id: ComputerSessionId,
    pub display_id: Option<ScreenId>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrameIdParams {
    pub session_id: ComputerSessionId,
    pub frame_id: FrameId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrameEncodeParams {
    pub session_id: ComputerSessionId,
    pub frame_id: FrameId,
    pub encoding: FrameEncoding,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrameCaptureResult {
    pub metadata: CaptureFrameMetadata,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FrameEncodeResult {
    pub screenshot: Screenshot,
    pub encoding: FrameEncoding,
    pub cache_hit: bool,
    pub encode_micros: u128,
    pub transport_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ActionParams {
    pub session_id: ComputerSessionId,
    pub action: ComputerAction,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticObserveParams {
    pub session_id: ComputerSessionId,
    pub window_id: WindowId,
    pub max_depth: Option<u32>,
    pub max_elements: Option<u32>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct CapabilityProbeParams {
    pub session_id: ComputerSessionId,
    pub window_id: WindowId,
}

pub type CapabilityProbeResponse = ApplicationCapabilityProfile;

impl SemanticObserveParams {
    pub fn limits(&self) -> SemanticObservationLimits {
        SemanticObservationLimits {
            max_depth: self
                .max_depth
                .unwrap_or(SemanticObservationLimits::default().max_depth),
            max_elements: self
                .max_elements
                .unwrap_or(SemanticObservationLimits::default().max_elements),
        }
        .bounded()
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticValidateParams {
    pub session_id: ComputerSessionId,
    pub element_id: ElementId,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SemanticActionParams {
    pub session_id: ComputerSessionId,
    pub action: SemanticAction,
}

pub type SemanticActionResponse = SemanticActionResult;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionPerformParams {
    pub session_id: ComputerSessionId,
    pub execution_request: ComputerExecutionRequest,
}

pub type ExecutionPerformResponse = ComputerExecutionResult;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ShutdownResult {
    pub closed_sessions: usize,
    pub stopped: bool,
}

#[derive(Debug)]
pub enum FrameError {
    Io(io::Error),
    Truncated,
    InvalidLength(u32),
    TooLarge(usize),
    InvalidUtf8,
    InvalidJson(String),
}

impl fmt::Display for FrameError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "frame I/O error: {error}"),
            Self::Truncated => f.write_str("truncated length-prefixed frame"),
            Self::InvalidLength(length) => write!(f, "invalid frame length: {length}"),
            Self::TooLarge(length) => write!(f, "frame exceeds maximum size: {length}"),
            Self::InvalidUtf8 => f.write_str("frame is not UTF-8"),
            Self::InvalidJson(error) => write!(f, "invalid JSON payload: {error}"),
        }
    }
}

impl std::error::Error for FrameError {}

impl From<io::Error> for FrameError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Read one little-endian `[u32 length][payload]` frame.
///
/// `Ok(None)` means clean EOF between frames.  A partial header/payload is an
/// error and must be treated as a failed transport, never as a successful
/// request.
pub fn read_frame<R: Read>(reader: &mut R) -> Result<Option<Vec<u8>>, FrameError> {
    let mut header = [0u8; 4];
    let first = reader.read(&mut header[..1])?;
    if first == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut header[1..]).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })?;
    let length = u32::from_le_bytes(header);
    let length = usize::try_from(length).unwrap_or(usize::MAX);
    if length == 0 {
        return Err(FrameError::InvalidLength(0));
    }
    if length > MAX_FRAME_BYTES {
        let mut remaining = length;
        let mut discard = [0u8; 8192];
        while remaining > 0 {
            let count = remaining.min(discard.len());
            reader.read_exact(&mut discard[..count]).map_err(|error| {
                if error.kind() == io::ErrorKind::UnexpectedEof {
                    FrameError::Truncated
                } else {
                    FrameError::Io(error)
                }
            })?;
            remaining -= count;
        }
        return Err(FrameError::TooLarge(length));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload).map_err(|error| {
        if error.kind() == io::ErrorKind::UnexpectedEof {
            FrameError::Truncated
        } else {
            FrameError::Io(error)
        }
    })?;
    Ok(Some(payload))
}

pub fn write_frame<W: Write, T: Serialize>(writer: &mut W, value: &T) -> Result<(), FrameError> {
    let payload =
        serde_json::to_vec(value).map_err(|error| FrameError::InvalidJson(error.to_string()))?;
    if payload.is_empty() {
        return Err(FrameError::InvalidLength(0));
    }
    if payload.len() > MAX_FRAME_BYTES || payload.len() > u32::MAX as usize {
        return Err(FrameError::TooLarge(payload.len()));
    }
    writer.write_all(&(payload.len() as u32).to_le_bytes())?;
    writer.write_all(&payload)?;
    writer.flush()?;
    Ok(())
}

pub fn decode_json<T: DeserializeOwned>(payload: &[u8]) -> Result<T, FrameError> {
    let text = std::str::from_utf8(payload).map_err(|_| FrameError::InvalidUtf8)?;
    serde_json::from_str(text).map_err(|error| FrameError::InvalidJson(error.to_string()))
}

pub fn ok_response<T: Serialize>(request_id: impl Into<String>, value: &T) -> RpcResponse {
    RpcResponse {
        request_id: request_id.into(),
        ok: true,
        result: Some(serde_json::to_value(value).expect("RPC result must be serializable")),
        error: None,
    }
}

pub fn error_response(request_id: impl Into<String>, error: RpcError) -> RpcResponse {
    RpcResponse {
        request_id: request_id.into(),
        ok: false,
        result: None,
        error: Some(error),
    }
}

pub fn computer_error(error: ComputerError) -> RpcError {
    let (code, retryable) = match &error {
        ComputerError::InvalidAction(_) => ("INVALID_ACTION", false),
        ComputerError::InvalidWindow(_) => ("INVALID_WINDOW", false),
        ComputerError::UnknownDisplay(_) => ("UNKNOWN_DISPLAY", false),
        ComputerError::StaleDisplay(_) => ("STALE_DISPLAY", false),
        ComputerError::StaleElement(_) => ("STALE_ELEMENT", false),
        ComputerError::UnknownElement(_) => ("UNKNOWN_ELEMENT", false),
        ComputerError::InvalidCoordinate(_) => ("INVALID_COORDINATE", false),
        ComputerError::SessionClosed => ("INVALID_SESSION", false),
        ComputerError::NotInitialized => ("NOT_INITIALIZED", false),
        ComputerError::AlreadyInitialized => ("ALREADY_INITIALIZED", false),
        ComputerError::ForegroundDenied(_) => ("FOREGROUND_DENIED", false),
        ComputerError::FocusNotAcquired(_) => ("FOCUS_NOT_ACQUIRED", false),
        ComputerError::Unsupported { .. } => ("UNSUPPORTED", false),
        ComputerError::CapabilityGap { .. } => ("CAPABILITY_GAP", false),
        ComputerError::IntegrityMismatch(_) => ("INTEGRITY_MISMATCH", false),
        ComputerError::ElevationRequired(_) => ("ELEVATION_REQUIRED", false),
        ComputerError::UipiDenied(_) => ("UIPI_DENIED", false),
        ComputerError::ProtectedDesktop(_) => ("PROTECTED_DESKTOP", false),
        ComputerError::SecurityContextUnavailable(_) => ("SECURITY_CONTEXT_UNAVAILABLE", false),
        ComputerError::AccessDenied(_) => ("ACCESS_DENIED", false),
        ComputerError::TargetUnavailable(_) => ("TARGET_UNAVAILABLE", false),
        ComputerError::Backend(_) => ("BACKEND_ERROR", false),
    };
    let outcome_unknown = matches!(
        &error,
        ComputerError::Backend(message) if message.starts_with("OUTCOME_UNKNOWN:")
    );
    RpcError::new(code, error.to_string())
        .retryable(retryable)
        .unknown_outcome(outcome_unknown)
}

pub fn invalid_params(message: impl Into<String>) -> RpcError {
    RpcError::new("INVALID_PARAMS", message)
}

pub fn transport_error(message: impl Into<String>, unknown_outcome: bool) -> RpcError {
    RpcError::new("TRANSPORT_ERROR", message)
        .retryable(!unknown_outcome)
        .unknown_outcome(unknown_outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn framing_round_trips_and_rejects_partial_payload() {
        let request = RpcRequest {
            request_id: "r1".into(),
            method: "health".into(),
            params: serde_json::json!({}),
        };
        let mut bytes = Vec::new();
        write_frame(&mut bytes, &request).unwrap();
        let payload = read_frame(&mut Cursor::new(bytes)).unwrap().unwrap();
        let decoded: RpcRequest = decode_json(&payload).unwrap();
        assert_eq!(decoded.request_id, "r1");

        let mut malformed = Vec::new();
        malformed.extend_from_slice(&3u32.to_le_bytes());
        malformed.extend_from_slice(b"{}");
        assert!(matches!(
            read_frame(&mut Cursor::new(malformed)),
            Err(FrameError::Truncated)
        ));
    }
}
