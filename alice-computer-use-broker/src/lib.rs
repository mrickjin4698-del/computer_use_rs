//! Host-owned Computer execution boundary.
//!
//! R1 deliberately owns request/session/reference lifecycle and exactly-once
//! dispatch. Later production stages add the desktop lease, interference,
//! policy, approval, audit, and event adapters around this boundary. No
//! caller outside this crate receives a spawnable sidecar client.

mod boundary;
mod input_activity;
mod policy;
mod telemetry;

pub use boundary::{route_surface, ComputerBoundaryRoute, ComputerSurfaceKind};
pub use policy::{
    BackgroundComputerAccess, ComputerApprovalProfile, ComputerPolicy, ComputerPolicyDecision,
    ComputerRiskLevel, HostProtectedSurface,
};
pub use telemetry::{
    ComputerAuditRecord, ComputerAuditSink, ComputerEvent, ComputerEventKind, ComputerEventSink,
    ComputerEvidenceMetadata, ComputerEvidenceMode, ComputerEvidenceStore,
    InMemoryComputerAuditSink, InMemoryComputerEventSink, NullComputerAuditSink,
    NullComputerEventSink,
};

use alice_computer_use_core::{
    ApplicationCapabilityProfile, ApplicationIdentity, CapabilityAssessment, CapabilityStatus,
    CaptureFrameMetadata, ComputerAction, ComputerExecutionIntent, ComputerExecutionOutcome,
    ComputerExecutionRequest, ComputerExecutionResult, ComputerObservation, ComputerSessionId,
    ElementId, FrameEncoding, FrameEncodingResult, FrameId, FrameMetadataResult, ScreenId,
    Screenshot, SemanticObservation, SemanticObservationLimits, Window, WindowId,
};
use alice_computer_use_sidecar_client::{
    ComputerHostError, ComputerHostService, ComputerHostServiceState, ComputerHostSession,
    ComputerHostStatus,
};
use input_activity::{InputActivityMonitor, InputActivitySnapshot, ALICE_COMPUTER_INPUT_MARKER};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct ComputerRequestOwner {
    pub application_session_id: String,
    pub agent_thread_id: String,
    pub turn_id: String,
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_task_id: Option<String>,
}

impl ComputerRequestOwner {
    pub fn new(
        application_session_id: impl Into<String>,
        agent_thread_id: impl Into<String>,
        turn_id: impl Into<String>,
        tool_call_id: impl Into<String>,
        background_task_id: Option<String>,
    ) -> Self {
        Self {
            application_session_id: application_session_id.into(),
            agent_thread_id: agent_thread_id.into(),
            turn_id: turn_id.into(),
            tool_call_id: tool_call_id.into(),
            background_task_id,
        }
    }

    fn validate(&self) -> Result<(), BrokerError> {
        for (name, value) in [
            ("application_session_id", &self.application_session_id),
            ("agent_thread_id", &self.agent_thread_id),
            ("turn_id", &self.turn_id),
            ("tool_call_id", &self.tool_call_id),
        ] {
            if value.trim().is_empty() {
                return Err(BrokerError::InvalidRequest(format!(
                    "{name} must not be empty"
                )));
            }
        }
        if self
            .background_task_id
            .as_deref()
            .is_some_and(|value| value.trim().is_empty())
        {
            return Err(BrokerError::InvalidRequest(
                "background_task_id must not be empty".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct BrokerSessionRef {
    pub computer_session_id: ComputerSessionId,
    pub generation: u64,
    pub owner: ComputerRequestOwner,
}

#[derive(Clone, Debug)]
pub struct BrokerExecutionRequest {
    pub request_id: String,
    pub session: BrokerSessionRef,
    pub lease: DesktopLeaseRef,
    pub execution: ComputerExecutionRequest,
}

/// Read-only admission data captured once for a bounded action batch. The
/// final target-identity and interaction checks still run for every dispatch;
/// foreground matching remains mandatory for takeover routes, while macOS
/// background routes validate the target process/window.
#[derive(Clone, Debug)]
pub struct BrokerExecutionBatchContext {
    session: BrokerSessionRef,
    lease: DesktopLeaseRef,
    windows: Vec<Window>,
    target: Option<WindowId>,
    target_window: Option<Window>,
    capability_profile: Option<ApplicationCapabilityProfile>,
}

impl BrokerExecutionBatchContext {
    pub fn target_window_id(&self) -> Option<WindowId> {
        self.target.clone()
    }

    pub fn target_application(&self) -> Option<ApplicationIdentity> {
        self.capability_profile
            .as_ref()
            .map(|profile| profile.application.clone())
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct DesktopLeaseRef {
    pub lease_id: String,
    pub generation: u64,
    pub computer_session_id: ComputerSessionId,
    pub owner: ComputerRequestOwner,
    /// Leases are intentionally short-lived; callers acquire one per action.
    pub expires_at_unix_ms: u64,
}

impl BrokerExecutionRequest {
    pub fn validate(&self) -> Result<(), BrokerError> {
        if self.request_id.trim().is_empty() || self.request_id.len() > 256 {
            return Err(BrokerError::InvalidRequest(
                "request_id must be 1..=256 bytes".into(),
            ));
        }
        self.session.owner.validate()
    }
}

#[derive(Debug)]
pub enum BrokerError {
    Host(ComputerHostError),
    InvalidRequest(String),
    HostUnavailable,
    SessionNotFound(ComputerSessionId),
    CrossSessionReference,
    StaleReference {
        expected_generation: u64,
        actual_generation: u64,
    },
    DesktopBusy,
    LeaseRequired,
    InvalidLease,
    StaleLease,
    UserTakeover,
    ExternalInputConflict,
    UserActivityUnknown(String),
    TargetForegroundChanged {
        target: Option<WindowId>,
        actual: Option<WindowId>,
    },
    ForegroundConflict {
        target: Option<WindowId>,
        actual: Option<WindowId>,
    },
    InteractionProbeUnavailable(String),
    ApprovalRequired {
        approval_id: String,
        risk: ComputerRiskLevel,
    },
    ApprovalDenied(String),
    ApprovalContextChanged(String),
    ApprovalFocusRestoreFailed(String),
    ProtectedSurface {
        target: Option<WindowId>,
    },
    ProtectedSurfaceReason {
        target: Option<WindowId>,
        reason: String,
    },
    SecurityBlocked(String),
    BackgroundAccessDenied,
    CapabilityDenied(String),
    CapabilityUnknown(String),
    CircuitOpen,
    DuplicateRequest(String),
    OutcomeUnknown {
        code: String,
        message: String,
    },
}

impl BrokerError {
    pub fn code(&self) -> &str {
        match self {
            Self::Host(error) => match error {
                ComputerHostError::ProtocolMismatch { .. } => "PROTOCOL_MISMATCH",
                ComputerHostError::CapabilityContractMismatch { .. } => {
                    "CAPABILITY_CONTRACT_MISMATCH"
                }
                ComputerHostError::ResourceUnavailable(_) => "COMPUTER_UNAVAILABLE",
                ComputerHostError::AlreadyRunning { .. } => "ALREADY_RUNNING",
                ComputerHostError::Sidecar(error) => error.code(),
            },
            Self::InvalidRequest(_) => "INVALID_REQUEST",
            Self::HostUnavailable => "COMPUTER_UNAVAILABLE",
            Self::SessionNotFound(_) => "INVALID_SESSION",
            Self::CrossSessionReference => "CROSS_SESSION_REFERENCE",
            Self::StaleReference { .. } => "STALE_REFERENCE",
            Self::DesktopBusy => "DESKTOP_BUSY",
            Self::LeaseRequired => "DESKTOP_LEASE_REQUIRED",
            Self::InvalidLease => "INVALID_DESKTOP_LEASE",
            Self::StaleLease => "STALE_DESKTOP_LEASE",
            Self::UserTakeover => "USER_TAKEOVER",
            Self::ExternalInputConflict => "EXTERNAL_INPUT_CONFLICT",
            Self::UserActivityUnknown(_) => "USER_ACTIVITY_UNKNOWN",
            Self::TargetForegroundChanged { .. } => "TARGET_FOREGROUND_CHANGED",
            Self::ForegroundConflict { .. } => "FOREGROUND_CONFLICT",
            Self::InteractionProbeUnavailable(_) => "INTERACTION_PROBE_UNAVAILABLE",
            Self::ApprovalRequired { .. } => "APPROVAL_REQUIRED",
            Self::ApprovalDenied(_) => "APPROVAL_DENIED",
            Self::ApprovalContextChanged(_) => "APPROVAL_CONTEXT_CHANGED",
            Self::ApprovalFocusRestoreFailed(_) => "APPROVAL_FOCUS_RESTORE_FAILED",
            Self::ProtectedSurface { .. } | Self::ProtectedSurfaceReason { .. } => {
                "PROTECTED_SURFACE"
            }
            Self::SecurityBlocked(_) => "SECURITY_BLOCKED",
            Self::BackgroundAccessDenied => "BACKGROUND_ACCESS_DENIED",
            Self::CapabilityDenied(_) => "CAPABILITY_DENIED",
            Self::CapabilityUnknown(_) => "CAPABILITY_UNKNOWN",
            Self::CircuitOpen => "COMPUTER_UNAVAILABLE",
            Self::DuplicateRequest(_) => "DUPLICATE_REQUEST",
            Self::OutcomeUnknown { .. } => "OUTCOME_UNKNOWN",
        }
    }

    pub fn outcome_unknown(&self) -> bool {
        matches!(self, Self::OutcomeUnknown { .. })
            || matches!(self, Self::Host(ComputerHostError::Sidecar(error)) if error.outcome_unknown())
    }

    pub fn retryable(&self) -> bool {
        matches!(self, Self::Host(ComputerHostError::Sidecar(error)) if error.retryable())
    }

    fn requires_pressed_cleanup(&self) -> bool {
        matches!(
            self,
            Self::UserTakeover
                | Self::ExternalInputConflict
                | Self::UserActivityUnknown(_)
                | Self::InteractionProbeUnavailable(_)
                | Self::TargetForegroundChanged { .. }
                | Self::ForegroundConflict { .. }
        ) || matches!(
            self,
            Self::OutcomeUnknown { code, .. }
                if matches!(
                    code.as_str(),
                    "USER_TAKEOVER"
                        | "EXTERNAL_INPUT_CONFLICT"
                        | "USER_ACTIVITY_UNKNOWN"
                        | "TARGET_FOREGROUND_CHANGED"
                )
        )
    }
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Host(error) => write!(f, "computer host: {error}"),
            Self::InvalidRequest(message) => write!(f, "invalid computer request: {message}"),
            Self::HostUnavailable => f.write_str("computer host is unavailable"),
            Self::SessionNotFound(id) => write!(f, "computer session not found: {id}"),
            Self::CrossSessionReference => f.write_str("computer reference belongs to another session"),
            Self::StaleReference { expected_generation, actual_generation } => write!(
                f,
                "computer reference is stale: expected generation {expected_generation}, actual {actual_generation}"
            ),
            Self::DesktopBusy => f.write_str("desktop writer lease is held by another owner"),
            Self::LeaseRequired => f.write_str("side-effect action requires a desktop writer lease"),
            Self::InvalidLease => f.write_str("desktop writer lease is invalid"),
            Self::StaleLease => f.write_str("desktop writer lease is stale"),
            Self::UserTakeover => f.write_str("hardware input revoked the computer interaction lease"),
            Self::ExternalInputConflict => {
                f.write_str("input from another injector conflicted with the computer interaction lease")
            }
            Self::UserActivityUnknown(message) => {
                write!(f, "input activity could not be attributed safely: {message}")
            }
            Self::TargetForegroundChanged { target, actual } => write!(
                f,
                "target foreground changed: target={target:?}, actual={actual:?}"
            ),
            Self::ForegroundConflict { target, actual } => write!(
                f,
                "foreground window conflict: target={target:?}, actual={actual:?}"
            ),
            Self::InteractionProbeUnavailable(message) => {
                write!(f, "interaction guard unavailable: {message}")
            }
            Self::ApprovalRequired { approval_id, risk } => {
                write!(f, "approval required for {risk:?}: {approval_id}")
            }
            Self::ApprovalDenied(id) => write!(f, "approval denied: {id}"),
            Self::ApprovalContextChanged(detail) => {
                write!(f, "approval context changed: {detail}")
            }
            Self::ApprovalFocusRestoreFailed(detail) => {
                write!(f, "approval focus restore failed: {detail}")
            }
            Self::ProtectedSurface { target } => {
                write!(f, "target is a protected host surface: {target:?}")
            }
            Self::ProtectedSurfaceReason { target, reason } => {
                write!(f, "target is a protected host surface ({reason}): {target:?}")
            }
            Self::SecurityBlocked(reason) => write!(f, "computer security policy blocked action: {reason}"),
            Self::BackgroundAccessDenied => {
                f.write_str("background computer side effects are not granted")
            }
            Self::CapabilityDenied(reason) => write!(f, "computer capability denied: {reason}"),
            Self::CapabilityUnknown(reason) => write!(f, "computer capability unknown: {reason}"),
            Self::CircuitOpen => f.write_str("computer supervisor circuit breaker is open"),
            Self::DuplicateRequest(id) => write!(f, "computer request was already dispatched: {id}"),
            Self::OutcomeUnknown { code, message } => write!(f, "{code}: {message}"),
        }
    }
}

impl std::error::Error for BrokerError {}

impl From<ComputerHostError> for BrokerError {
    fn from(error: ComputerHostError) -> Self {
        if let ComputerHostError::Sidecar(sidecar) = &error {
            if sidecar.outcome_unknown() {
                return Self::OutcomeUnknown {
                    code: sidecar.code().to_owned(),
                    message: sidecar.to_string(),
                };
            }
        }
        Self::Host(error)
    }
}

struct BrokerSessionRecord {
    reference: BrokerSessionRef,
    session: ComputerHostSession,
}

struct BrokerState {
    generation: u64,
    invalidated: bool,
    sessions: HashMap<ComputerSessionId, BrokerSessionRecord>,
    lease: Option<DesktopLeaseRef>,
    next_lease_id: u64,
    dispatched: HashSet<(ComputerSessionId, String)>,
    crash_restart_count: u32,
}

/// Backend-neutral state held only while one Computer approval is pending.
/// It deliberately stores no raw HWND/UIA runtime id and is consumed exactly
/// once by `resume_after_approval` or `discard_approval_context`.
#[derive(Clone, Debug)]
struct ApprovalReturnContext {
    session: BrokerSessionRef,
    request: ComputerExecutionRequest,
    target_window: Option<Window>,
    target_application: Option<ApplicationIdentity>,
    target_security: Option<alice_computer_use_core::WindowSecurityMetadata>,
    original_foreground: Option<WindowId>,
    semantic_generation: Option<u64>,
    topology_generation: Option<u64>,
    action_kind: String,
    approval_id: String,
    risk: ComputerRiskLevel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(clippy::enum_variant_names)]
enum ForegroundTransition {
    ExpectedHostTransition,
    ExpectedAgentTransition,
    UnexpectedTransition,
}

impl ForegroundTransition {
    fn as_str(self) -> &'static str {
        match self {
            Self::ExpectedHostTransition => "expected_host_transition",
            Self::ExpectedAgentTransition => "expected_agent_transition",
            Self::UnexpectedTransition => "unexpected_transition",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InteractionState {
    NoConflict,
    UserTakeover,
    ExternalInputConflict,
    ActivityUnknown,
    ContextChanged,
}

/// Runtime-facing UX projection for interaction ownership.
///
/// `UserTakeover` is deliberately kept as the broker's durable reason while
/// the UX consumes the more descriptive pause transition: the broker yields
/// first, then remains paused until a fresh observation establishes a new
/// baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerUxState {
    NoConflict,
    Yielding,
    PausedByUser,
    ContextChanged,
    ExternalInputConflict,
    ActivityUnknown,
}

impl InteractionState {
    pub fn ux_state(self) -> ComputerUxState {
        match self {
            Self::NoConflict => ComputerUxState::NoConflict,
            Self::UserTakeover => ComputerUxState::PausedByUser,
            Self::ExternalInputConflict => ComputerUxState::ExternalInputConflict,
            Self::ActivityUnknown => ComputerUxState::ActivityUnknown,
            Self::ContextChanged => ComputerUxState::ContextChanged,
        }
    }

    pub fn ux_transition(self) -> Option<ComputerUxState> {
        match self {
            Self::NoConflict => None,
            Self::UserTakeover => Some(ComputerUxState::Yielding),
            Self::ExternalInputConflict => Some(ComputerUxState::ExternalInputConflict),
            Self::ActivityUnknown => Some(ComputerUxState::ActivityUnknown),
            Self::ContextChanged => Some(ComputerUxState::ContextChanged),
        }
    }
}

impl fmt::Display for ComputerUxState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoConflict => "no_conflict",
            Self::Yielding => "yielding",
            Self::PausedByUser => "paused_by_user",
            Self::ContextChanged => "context_changed",
            Self::ExternalInputConflict => "external_input_conflict",
            Self::ActivityUnknown => "activity_unknown",
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InteractionActionClass {
    TypeText,
    SpatialTypeText,
    Key,
    Click,
    PointerGesture,
    HeldPointerDown,
    HeldPointerUp,
    HeldKeyDown,
    HeldKeyUp,
    FocusWindow,
    Semantic,
}

impl InteractionActionClass {
    fn blocks_hardware_mouse_move(self) -> bool {
        matches!(
            self,
            Self::SpatialTypeText
                | Self::PointerGesture
                | Self::HeldPointerDown
                | Self::HeldPointerUp
        )
    }

    fn begins_held_gesture(self) -> bool {
        matches!(self, Self::HeldPointerDown | Self::HeldKeyDown)
    }

    fn ends_held_gesture(self) -> bool {
        matches!(self, Self::HeldPointerUp | Self::HeldKeyUp)
    }
}

#[derive(Debug)]
struct InteractionGuardState {
    state: InteractionState,
}

#[derive(Clone, Debug)]
struct ComputerInteractionLease {
    lease_id: String,
    session_id: ComputerSessionId,
    owner: ComputerRequestOwner,
    target: Option<WindowId>,
    /// Keyboard/text input can remain bound to the same process while its
    /// top-level window changes (for example after a browser opens a tab).
    /// Pointer input continues to use the exact `target` window below.
    target_application: Option<ApplicationIdentity>,
    action: InteractionActionClass,
    created_at: Instant,
    baseline: InputActivitySnapshot,
    last_alice_input_sequence: u64,
    last_hardware_input_sequence: u64,
}

/// Host-owned, short-lived admission guard for one desktop write action.
///
/// It deliberately does not infer takeover from the system idle timer. A
/// low-level monitor attributes hardware and injected activity, while every
/// side-effect dispatch still requires a fresh target/interaction check;
/// takeover routes additionally require a fresh foreground check.
pub struct ComputerInteractionGuard {
    state: Mutex<InteractionGuardState>,
    monitor: Arc<InputActivityMonitor>,
    held_gesture_baselines: Mutex<HashMap<ComputerSessionId, InputActivitySnapshot>>,
}

impl Default for ComputerInteractionGuard {
    fn default() -> Self {
        Self::new(Arc::new(InputActivityMonitor::new(
            ALICE_COMPUTER_INPUT_MARKER,
        )))
    }
}

impl ComputerInteractionGuard {
    fn new(monitor: Arc<InputActivityMonitor>) -> Self {
        Self {
            state: Mutex::new(InteractionGuardState {
                state: if monitor.available() {
                    InteractionState::NoConflict
                } else {
                    InteractionState::ActivityUnknown
                },
            }),
            monitor,
            held_gesture_baselines: Mutex::new(HashMap::new()),
        }
    }

    pub fn state(&self) -> InteractionState {
        self.state.lock().expect("interaction guard poisoned").state
    }

    pub fn monitor_available(&self) -> bool {
        self.monitor.available()
    }

    /// Establish an observation baseline. Observation remains read-only even
    /// if the activity monitor is unavailable; only side-effect admission is
    /// fail-closed.
    fn observe(&self) {
        let mut guard = self.state.lock().expect("interaction guard poisoned");
        guard.state = if self.monitor.available() {
            InteractionState::NoConflict
        } else {
            InteractionState::ActivityUnknown
        };
    }

    fn release_for_approval(&self, session_id: &ComputerSessionId) {
        self.clear_gesture(session_id);
        self.observe();
    }

    fn reset_after_interrupt(&self) {
        self.clear_all_gestures();
        self.observe();
    }

    fn begin_lease(
        &self,
        lease: &DesktopLeaseRef,
        session: &BrokerSessionRef,
        action: InteractionActionClass,
        target: Option<WindowId>,
    ) -> Result<ComputerInteractionLease, BrokerError> {
        let current = self.monitor.snapshot();
        if !current.monitor_available {
            self.set_state(InteractionState::ActivityUnknown);
            return Err(BrokerError::UserActivityUnknown(
                "low-level input monitor is unavailable".into(),
            ));
        }
        let baseline = self
            .held_gesture_baselines
            .lock()
            .expect("interaction gesture state poisoned")
            .get(&session.computer_session_id)
            .copied()
            .unwrap_or(current);
        Ok(ComputerInteractionLease {
            lease_id: lease.lease_id.clone(),
            session_id: session.computer_session_id.clone(),
            owner: session.owner.clone(),
            target,
            target_application: None,
            action,
            created_at: Instant::now(),
            last_alice_input_sequence: baseline.alice_injected_sequence,
            last_hardware_input_sequence: baseline.hardware_sequence,
            baseline,
        })
    }

    fn preflight(
        &self,
        lease: &ComputerInteractionLease,
        windows: &[Window],
        target: Option<&WindowId>,
        focus_window_action: bool,
        allow_background: bool,
    ) -> Result<Option<WindowId>, BrokerError> {
        let actual = foreground_window(windows);
        if actual.is_none() {
            self.set_state(InteractionState::ActivityUnknown);
            return Err(BrokerError::InteractionProbeUnavailable(
                "window observation did not identify a foreground window".into(),
            ));
        }
        let expected_target = target.or(lease.target.as_ref());
        let application_matches = lease
            .target_application
            .as_ref()
            .is_some_and(|application| {
                application.process_id.is_some_and(|process_id| {
                    windows.iter().any(|window| {
                        (allow_background || window.active) && window.process_id == Some(process_id)
                    })
                })
            });
        if !allow_background
            && !focus_window_action
            && !application_matches
            && expected_target.is_some()
            && actual.as_ref() != expected_target
        {
            self.set_state(InteractionState::ContextChanged);
            return Err(BrokerError::TargetForegroundChanged {
                target: expected_target.cloned(),
                actual,
            });
        }
        self.check_activity(lease, false)?;
        self.set_state(InteractionState::NoConflict);
        Ok(actual)
    }

    fn postflight(&self, lease: &mut ComputerInteractionLease) -> Result<(), BrokerError> {
        let result = self.check_activity(lease, true).map(|_| ());
        let current = self.monitor.snapshot();
        lease.last_alice_input_sequence = current.alice_injected_sequence;
        lease.last_hardware_input_sequence = current.hardware_sequence;
        result
    }

    fn commit_gesture(&self, session_id: &ComputerSessionId, lease: &ComputerInteractionLease) {
        let mut baselines = self
            .held_gesture_baselines
            .lock()
            .expect("interaction gesture state poisoned");
        if lease.action.begins_held_gesture() {
            baselines.insert(session_id.clone(), self.monitor.snapshot());
        } else if lease.action.ends_held_gesture() {
            baselines.remove(session_id);
        }
    }

    fn clear_gesture(&self, session_id: &ComputerSessionId) {
        self.held_gesture_baselines
            .lock()
            .expect("interaction gesture state poisoned")
            .remove(session_id);
    }

    fn clear_all_gestures(&self) {
        self.held_gesture_baselines
            .lock()
            .expect("interaction gesture state poisoned")
            .clear();
    }

    fn check_activity(
        &self,
        lease: &ComputerInteractionLease,
        dispatched: bool,
    ) -> Result<(), BrokerError> {
        let current = self.monitor.snapshot();
        if !current.monitor_available {
            self.set_state(InteractionState::ActivityUnknown);
            return Err(if dispatched {
                BrokerError::OutcomeUnknown {
                    code: "USER_ACTIVITY_UNKNOWN".into(),
                    message: "input monitor became unavailable after dispatch".into(),
                }
            } else {
                BrokerError::UserActivityUnknown("low-level input monitor is unavailable".into())
            });
        }
        if current.unknown_sequence > lease.baseline.unknown_sequence {
            self.set_state(InteractionState::ActivityUnknown);
            return Err(if dispatched {
                BrokerError::OutcomeUnknown {
                    code: "USER_ACTIVITY_UNKNOWN".into(),
                    message: "input event attribution became unknown".into(),
                }
            } else {
                BrokerError::UserActivityUnknown("input event attribution became unknown".into())
            });
        }
        let other_keyboard = current.other_injected_keyboard_sequence
            > lease.baseline.other_injected_keyboard_sequence;
        let other_button = current.other_injected_mouse_button_sequence
            > lease.baseline.other_injected_mouse_button_sequence;
        let other_wheel = current.other_injected_mouse_wheel_sequence
            > lease.baseline.other_injected_mouse_wheel_sequence;
        let other_move = current.other_injected_mouse_move_sequence
            > lease.baseline.other_injected_mouse_move_sequence;
        if other_keyboard
            || other_button
            || other_wheel
            || (other_move && lease.action.blocks_hardware_mouse_move())
        {
            self.set_state(InteractionState::ExternalInputConflict);
            let message = format!(
                "another injector overlapped lease={} session={} age_ms={} alice_seq={}->{} other_seq={}->{} hardware_seq={}->{} keyboard={} button={} wheel={} move={}",
                lease.lease_id,
                lease.session_id,
                lease.created_at.elapsed().as_millis(),
                lease.last_alice_input_sequence,
                current.alice_injected_sequence,
                lease.baseline.other_injected_sequence,
                current.other_injected_sequence,
                lease.last_hardware_input_sequence,
                current.hardware_sequence,
                other_keyboard,
                other_button,
                other_wheel,
                other_move,
            );
            return Err(if dispatched {
                BrokerError::OutcomeUnknown {
                    code: "EXTERNAL_INPUT_CONFLICT".into(),
                    message,
                }
            } else {
                BrokerError::ExternalInputConflict
            });
        }
        let hardware_keyboard =
            current.hardware_keyboard_sequence > lease.baseline.hardware_keyboard_sequence;
        let hardware_button =
            current.hardware_mouse_button_sequence > lease.baseline.hardware_mouse_button_sequence;
        let hardware_wheel =
            current.hardware_mouse_wheel_sequence > lease.baseline.hardware_mouse_wheel_sequence;
        let hardware_move =
            current.hardware_mouse_move_sequence > lease.baseline.hardware_mouse_move_sequence;
        if hardware_keyboard
            || hardware_button
            || hardware_wheel
            || (hardware_move && lease.action.blocks_hardware_mouse_move())
        {
            self.set_state(InteractionState::UserTakeover);
            let message = format!(
                "hardware input overlapped lease={} owner_tool={} action={:?} alice_seq={} hardware_seq={}",
                lease.lease_id,
                lease.owner.tool_call_id,
                lease.action,
                lease.last_alice_input_sequence,
                lease.last_hardware_input_sequence
            );
            return Err(if dispatched {
                BrokerError::OutcomeUnknown {
                    code: "USER_TAKEOVER".into(),
                    message,
                }
            } else {
                BrokerError::UserTakeover
            });
        }
        Ok(())
    }

    fn complete(&self) {
        let mut guard = self.state.lock().expect("interaction guard poisoned");
        if !matches!(
            guard.state,
            InteractionState::UserTakeover
                | InteractionState::ExternalInputConflict
                | InteractionState::ActivityUnknown
                | InteractionState::ContextChanged
        ) {
            guard.state = InteractionState::NoConflict;
        }
    }

    fn set_state(&self, state: InteractionState) {
        self.state.lock().expect("interaction guard poisoned").state = state;
    }
}

fn foreground_window(windows: &[Window]) -> Option<WindowId> {
    windows
        .iter()
        .find(|window| window.active)
        .map(|window| window.id.clone())
}

fn is_host_surface(window: &Window) -> bool {
    window.process_id == Some(std::process::id())
}

fn foreground_transition(
    context: &ApprovalReturnContext,
    actual: Option<&Window>,
    target: &WindowId,
) -> ForegroundTransition {
    let Some(actual) = actual else {
        return ForegroundTransition::UnexpectedTransition;
    };
    if is_host_surface(actual) {
        return ForegroundTransition::ExpectedHostTransition;
    }
    if actual.id == *target {
        return if context.original_foreground.as_ref() == Some(&actual.id) {
            ForegroundTransition::ExpectedHostTransition
        } else {
            ForegroundTransition::ExpectedAgentTransition
        };
    }
    if context.original_foreground.as_ref() == Some(&actual.id) {
        return ForegroundTransition::ExpectedHostTransition;
    }
    ForegroundTransition::UnexpectedTransition
}

fn same_target_identity(
    expected: &Window,
    actual: &Window,
    request: &ComputerExecutionRequest,
) -> bool {
    if expected.id != actual.id
        || expected.process_id != actual.process_id
        || expected.security != actual.security
    {
        return false;
    }
    // Pixel requests carry coordinates/bounds derived from the pre-approval
    // observation. A moved/resized target is therefore stale and cannot be
    // replayed against the old geometry.
    matches!(&request.intent, ComputerExecutionIntent::Semantic(_))
        || expected.bounds == actual.bounds
}

fn same_batch_target_identity(
    expected: &Window,
    actual: &Window,
    request: &ComputerExecutionRequest,
) -> bool {
    if expected.id != actual.id
        || expected.process_id != actual.process_id
        || expected.security != actual.security
    {
        return false;
    }
    let geometry_bound = matches!(
        &request.intent,
        ComputerExecutionIntent::Pixel {
            action: ComputerAction::Click { .. }
                | ComputerAction::DoubleClick { .. }
                | ComputerAction::RightClick { .. }
                | ComputerAction::MovePointer { .. }
                | ComputerAction::Drag { .. }
                | ComputerAction::Scroll { .. }
                | ComputerAction::MouseDown { .. }
                | ComputerAction::MouseUp { .. }
                | ComputerAction::MiddleClick { .. }
                | ComputerAction::TripleClick { .. }
                | ComputerAction::ModifierClick { .. }
                | ComputerAction::ModifiedPointer { .. }
                | ComputerAction::TypeText { at: Some(_), .. },
            ..
        }
    );
    !geometry_bound || expected.bounds == actual.bounds
}

#[derive(Clone)]
pub struct ComputerExecutionBroker {
    host: ComputerHostService,
    state: Arc<RwLock<BrokerState>>,
    policy: Arc<RwLock<ComputerPolicy>>,
    event_sink: Arc<RwLock<Arc<dyn ComputerEventSink>>>,
    audit_sink: Arc<RwLock<Arc<dyn ComputerAuditSink>>>,
    evidence: ComputerEvidenceStore,
    approval_contexts: Arc<Mutex<HashMap<String, ApprovalReturnContext>>>,
    action_gate: Arc<Mutex<()>>,
    interaction: Arc<ComputerInteractionGuard>,
}

impl ComputerExecutionBroker {
    pub fn new(host: ComputerHostService) -> Self {
        Self::with_sinks(
            host,
            Arc::new(NullComputerEventSink),
            Arc::new(NullComputerAuditSink),
        )
    }

    pub fn with_sinks(
        host: ComputerHostService,
        event_sink: Arc<dyn ComputerEventSink>,
        audit_sink: Arc<dyn ComputerAuditSink>,
    ) -> Self {
        let input_marker = host.input_marker();
        Self {
            host,
            state: Arc::new(RwLock::new(BrokerState {
                generation: 0,
                invalidated: false,
                sessions: HashMap::new(),
                lease: None,
                next_lease_id: 1,
                dispatched: HashSet::new(),
                crash_restart_count: 0,
            })),
            policy: Arc::new(RwLock::new(ComputerPolicy::default())),
            event_sink: Arc::new(RwLock::new(event_sink)),
            audit_sink: Arc::new(RwLock::new(audit_sink)),
            evidence: ComputerEvidenceStore::default(),
            approval_contexts: Arc::new(Mutex::new(HashMap::new())),
            action_gate: Arc::new(Mutex::new(())),
            interaction: Arc::new(ComputerInteractionGuard::new(Arc::new(
                InputActivityMonitor::new(input_marker),
            ))),
        }
    }

    pub fn host(&self) -> &ComputerHostService {
        &self.host
    }

    pub fn policy(&self) -> ComputerPolicy {
        self.policy
            .read()
            .expect("computer policy poisoned")
            .clone()
    }

    pub fn configure_policy(&self, policy: ComputerPolicy) {
        *self.policy.write().expect("computer policy poisoned") = policy;
    }

    pub fn record_approval(&self, approval_id: impl Into<String>) {
        self.policy
            .write()
            .expect("computer policy poisoned")
            .record_approval(approval_id);
    }

    pub fn revoke_approval(&self, approval_id: &str) {
        self.policy
            .write()
            .expect("computer policy poisoned")
            .revoke_approval(approval_id);
    }

    pub fn interaction_state(&self) -> InteractionState {
        self.interaction.state()
    }

    pub fn interaction_ux_state(&self) -> ComputerUxState {
        self.interaction.state().ux_state()
    }

    /// Exposes monitor health for host diagnostics. Read-only observation and
    /// screenshots remain available when this is false; side-effect actions
    /// fail closed with USER_ACTIVITY_UNKNOWN.
    pub fn input_activity_monitor_available(&self) -> bool {
        self.interaction.monitor_available()
    }

    pub fn install_event_sink(&self, sink: Arc<dyn ComputerEventSink>) {
        *self
            .event_sink
            .write()
            .expect("computer event sink poisoned") = sink;
    }

    pub fn install_audit_sink(&self, sink: Arc<dyn ComputerAuditSink>) {
        *self
            .audit_sink
            .write()
            .expect("computer audit sink poisoned") = sink;
    }

    pub fn configure_debug_evidence(
        &self,
        mode: ComputerEvidenceMode,
        max_frames: usize,
        max_bytes: usize,
        ttl: Duration,
    ) {
        self.evidence.configure(mode, max_frames, max_bytes, ttl);
    }

    pub fn record_debug_evidence(
        &self,
        phase: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Option<ComputerEvidenceMetadata> {
        self.evidence.record(phase, bytes)
    }

    pub fn debug_evidence_metadata(&self) -> Vec<ComputerEvidenceMetadata> {
        self.evidence.metadata()
    }

    pub fn clear_debug_evidence(&self) {
        self.evidence.clear();
    }

    pub fn status(&self) -> ComputerHostStatus {
        let status = self.host.status();
        let mut state = self.state.write().expect("computer broker poisoned");
        self.sync_locked(&mut state, &status);
        status
    }

    pub fn health(&self) -> Result<alice_computer_use_sidecar_client::HealthResult, BrokerError> {
        let health = self.host.health().map_err(BrokerError::from)?;
        let status = self.host.status();
        let mut state = self.state.write().expect("computer broker poisoned");
        self.sync_locked(&mut state, &status);
        Ok(health)
    }

    pub fn open_session(
        &self,
        owner: ComputerRequestOwner,
    ) -> Result<BrokerSessionRef, BrokerError> {
        owner.validate()?;
        let session = self.host.open_session().map_err(BrokerError::from)?;
        let status = self.host.status();
        if status.state != ComputerHostServiceState::Ready {
            return Err(BrokerError::HostUnavailable);
        }
        let reference = BrokerSessionRef {
            computer_session_id: session.id().clone(),
            generation: status.generation,
            owner,
        };
        let mut state = self.state.write().expect("computer broker poisoned");
        self.sync_locked(&mut state, &status);
        state.generation = status.generation;
        state.invalidated = false;
        state.sessions.insert(
            reference.computer_session_id.clone(),
            BrokerSessionRecord {
                reference: reference.clone(),
                session,
            },
        );
        self.emit_event(ComputerEvent {
            event_type: ComputerEventKind::SessionStarted,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(reference.computer_session_id.to_string()),
            owner: Some(reference.owner.clone()),
            request_id: None,
            action_kind: None,
            outcome: None,
            detail: None,
            ux_state: None,
            ux_transition: None,
        });
        Ok(reference)
    }

    pub fn close_session(&self, reference: &BrokerSessionRef) -> Result<(), BrokerError> {
        self.refresh_state();
        let session = {
            let mut state = self.state.write().expect("computer broker poisoned");
            let session = self.validate_locked(&state, reference)?.session.clone();
            state.sessions.remove(&reference.computer_session_id);
            state
                .dispatched
                .retain(|(session_id, _)| session_id != &reference.computer_session_id);
            if state
                .lease
                .as_ref()
                .is_some_and(|lease| lease.computer_session_id == reference.computer_session_id)
            {
                state.lease = None;
            }
            session
        };
        self.interaction
            .clear_gesture(&reference.computer_session_id);
        self.clear_approval_contexts_for_session(&reference.computer_session_id);
        session.close().map_err(BrokerError::from)
    }

    pub fn restart_after_crash(&self) -> Result<ComputerHostStatus, BrokerError> {
        let status = self.host.status();
        {
            let mut state = self.state.write().expect("computer broker poisoned");
            if state.crash_restart_count >= 3 {
                return Err(BrokerError::CircuitOpen);
            }
            state.crash_restart_count = state.crash_restart_count.saturating_add(1);
            self.sync_locked(&mut state, &status);
            state.sessions.clear();
            state.lease = None;
            state.dispatched.clear();
            state.invalidated = true;
        }
        self.interaction.clear_all_gestures();
        self.clear_all_approval_contexts();
        let status = self.host.restart_after_crash().map_err(BrokerError::from)?;
        self.emit_event(ComputerEvent {
            event_type: ComputerEventKind::SidecarRestarted,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: None,
            owner: None,
            request_id: None,
            action_kind: None,
            outcome: None,
            detail: Some(format!("generation={}", status.generation)),
            ux_state: None,
            ux_transition: None,
        });
        Ok(status)
    }

    /// Read-only recovery is explicit and creates a new logical session. It
    /// never retries or replays an action that may have been in flight.
    pub fn recover_read_only(
        &self,
        owner: ComputerRequestOwner,
    ) -> Result<BrokerSessionRef, BrokerError> {
        if self.status().state != ComputerHostServiceState::Ready {
            self.restart_after_crash()?;
        }
        self.open_session(owner)
    }

    pub fn shutdown(&self) -> Result<(), BrokerError> {
        let sessions = {
            let mut state = self.state.write().expect("computer broker poisoned");
            state.lease = None;
            state.dispatched.clear();
            state
                .sessions
                .drain()
                .map(|(_, record)| record.session)
                .collect::<Vec<_>>()
        };
        self.interaction.clear_all_gestures();
        self.clear_all_approval_contexts();
        for session in sessions {
            let _ = session.close();
        }
        self.host.shutdown().map_err(BrokerError::from)
    }

    /// Stop Turn is a desktop-input cancellation boundary. The next action
    /// must acquire a fresh lease and establish a fresh interaction baseline;
    /// it must not inherit pressed-state or UX state from the interrupted
    /// action.
    pub fn reset_after_interrupt(&self) {
        if let Ok(mut state) = self.state.write() {
            state.lease = None;
        }
        self.interaction.reset_after_interrupt();
        self.clear_all_approval_contexts();
    }

    pub fn window_list(&self, reference: &BrokerSessionRef) -> Result<Vec<Window>, BrokerError> {
        self.with_session(reference, |session| session.window_list())
    }

    /// Prepare one bounded batch from a fresh broker-owned window snapshot.
    /// The snapshot is never the only final admission decision: each action
    /// still performs a final window-list/preflight immediately before
    /// dispatch, and the target identity is revalidated against this snapshot.
    pub fn prepare_execution_batch(
        &self,
        reference: &BrokerSessionRef,
        lease: &DesktopLeaseRef,
        requested_target: Option<WindowId>,
        probe_capability: bool,
        allow_background: bool,
    ) -> Result<BrokerExecutionBatchContext, BrokerError> {
        self.refresh_state();
        let session = {
            let state = self.state.read().expect("computer broker poisoned");
            let session = self.validate_locked(&state, reference)?.session.clone();
            self.validate_lease_locked(&state, reference, lease)?;
            session
        };
        let windows = session.window_list().map_err(BrokerError::from)?;
        let active = foreground_window(&windows);
        let target = match requested_target {
            Some(target) => {
                let target_window = windows.iter().find(|window| window.id == target);
                let Some(target_window) = target_window else {
                    return Err(BrokerError::InvalidRequest(
                        "target window is not present in the fresh window list".into(),
                    ));
                };
                if !allow_background && !target_window.active {
                    return Err(BrokerError::TargetForegroundChanged {
                        target: Some(target),
                        actual: active,
                    });
                }
                Some(target_window.id.clone())
            }
            None => active,
        };
        let target_window = target
            .as_ref()
            .and_then(|target| windows.iter().find(|window| &window.id == target))
            .cloned();
        let capability_profile = if probe_capability {
            target_window
                .as_ref()
                .map(|window| session.capability_probe(&window.id))
                .transpose()
                .map_err(BrokerError::from)?
        } else {
            None
        };
        Ok(BrokerExecutionBatchContext {
            session: reference.clone(),
            lease: lease.clone(),
            windows,
            target,
            target_window,
            capability_profile,
        })
    }

    /// Dispatch one request using a previously prepared bounded-batch
    /// context. The request and lease must belong to that exact context.
    pub fn execute_with_profile_in_batch(
        &self,
        request: BrokerExecutionRequest,
        profile: ComputerApprovalProfile,
        context: &BrokerExecutionBatchContext,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        if request.session != context.session || request.lease != context.lease {
            return Err(BrokerError::InvalidRequest(
                "execution request does not belong to the prepared batch context".into(),
            ));
        }
        let requested_target = request_target_window(&request.execution)
            .or_else(|| requested_interaction_target(&request.execution));
        let application_matches = request
            .execution
            .target_application()
            .and_then(|application| application.process_id)
            .zip(
                context
                    .target_window
                    .as_ref()
                    .and_then(|window| window.process_id),
            )
            .is_some_and(|(expected, actual)| expected == actual);
        if request.execution.target_application().is_some() {
            if !application_matches {
                return Err(BrokerError::InvalidRequest(
                    "application target does not match the prepared foreground application".into(),
                ));
            }
        } else if requested_target.as_ref() != context.target.as_ref() && requested_target.is_some()
        {
            return Err(BrokerError::InvalidRequest(
                "execution target does not match the prepared batch context".into(),
            ));
        }
        let mut policy = self.policy();
        policy.set_profile(profile);
        let forced_target = (!is_unscoped_keyboard_input(&request.execution))
            .then(|| context.target.clone())
            .flatten();
        self.execute_with_policy_bound(request, &policy, forced_target, Some(context))
    }

    pub fn observe(
        &self,
        reference: &BrokerSessionRef,
    ) -> Result<ComputerObservation, BrokerError> {
        let observation = self.with_session(reference, |session| session.observe())?;
        self.interaction.observe();
        self.emit_event(ComputerEvent {
            event_type: ComputerEventKind::ObservationCompleted,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(reference.computer_session_id.to_string()),
            owner: Some(reference.owner.clone()),
            request_id: None,
            action_kind: None,
            outcome: Some(format!(
                "windows={} screens={} screenshot={}",
                observation.windows.len(),
                observation.screens.len(),
                observation.screenshot.is_some()
            )),
            detail: None,
            ux_state: Some(self.interaction_ux_state().to_string()),
            ux_transition: None,
        });
        Ok(observation)
    }

    pub fn capability_probe(
        &self,
        reference: &BrokerSessionRef,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, BrokerError> {
        self.with_session(reference, |session| session.capability_probe(window_id))
    }

    pub fn semantic_observe(
        &self,
        reference: &BrokerSessionRef,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, BrokerError> {
        self.with_session(reference, |session| {
            session.semantic_observe(window_id, limits)
        })
    }

    pub fn validate_element(
        &self,
        reference: &BrokerSessionRef,
        element_id: &ElementId,
    ) -> Result<(), BrokerError> {
        self.with_session(reference, |session| session.validate_element(element_id))
    }

    pub fn resolve_element_window(
        &self,
        reference: &BrokerSessionRef,
        element_id: &ElementId,
    ) -> Result<WindowId, BrokerError> {
        self.with_session(reference, |session| {
            session.resolve_element_window(element_id)
        })
    }

    pub fn capture_frame(
        &self,
        reference: &BrokerSessionRef,
        screen_id: Option<ScreenId>,
    ) -> Result<CaptureFrameMetadata, BrokerError> {
        self.with_session(reference, |session| session.capture_frame(screen_id))
    }

    pub fn frame_metadata(
        &self,
        reference: &BrokerSessionRef,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, BrokerError> {
        self.with_session(reference, |session| session.frame_metadata(frame_id))
    }

    pub fn encode_frame(
        &self,
        reference: &BrokerSessionRef,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, BrokerError> {
        self.with_session(reference, |session| {
            session.encode_frame(frame_id, encoding)
        })
    }

    pub fn release_frame(
        &self,
        reference: &BrokerSessionRef,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, BrokerError> {
        self.with_session(reference, |session| session.release_frame(frame_id))
    }

    pub fn screenshot(
        &self,
        reference: &BrokerSessionRef,
        screen_id: Option<ScreenId>,
    ) -> Result<Screenshot, BrokerError> {
        self.with_session(reference, |session| session.screenshot(screen_id))
    }

    pub fn acquire_desktop_lease(
        &self,
        reference: &BrokerSessionRef,
    ) -> Result<DesktopLeaseRef, BrokerError> {
        self.refresh_state();
        let mut state = self.state.write().expect("computer broker poisoned");
        self.validate_locked(&state, reference)?;
        if let Some(lease) = state.lease.as_ref() {
            if lease.computer_session_id == reference.computer_session_id
                && lease.generation == reference.generation
                && lease.owner == reference.owner
            {
                return Ok(lease.clone());
            }
            return Err(BrokerError::DesktopBusy);
        }
        let lease = DesktopLeaseRef {
            lease_id: format!("computer-desktop-lease-{}", state.next_lease_id),
            generation: reference.generation,
            computer_session_id: reference.computer_session_id.clone(),
            owner: reference.owner.clone(),
            expires_at_unix_ms: telemetry::now_unix_ms().saturating_add(15_000),
        };
        state.next_lease_id = state.next_lease_id.saturating_add(1);
        state.lease = Some(lease.clone());
        Ok(lease)
    }

    pub fn release_desktop_lease(&self, lease: &DesktopLeaseRef) -> Result<(), BrokerError> {
        self.refresh_state();
        let mut state = self.state.write().expect("computer broker poisoned");
        let Some(current) = state.lease.as_ref() else {
            return Err(BrokerError::InvalidLease);
        };
        if current.lease_id != lease.lease_id {
            return if current.generation != lease.generation {
                Err(BrokerError::StaleLease)
            } else {
                Err(BrokerError::InvalidLease)
            };
        }
        state.lease = None;
        Ok(())
    }

    pub fn execute(
        &self,
        request: BrokerExecutionRequest,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        let policy = self.policy();
        self.execute_with_policy(request, &policy)
    }

    /// Resume one approval-bound action. The pending context is consumed before
    /// any focus restore or business dispatch, so a resume can never replay an
    /// already-dispatched request.
    pub fn resume_after_approval(
        &self,
        request: BrokerExecutionRequest,
        profile: ComputerApprovalProfile,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        request.validate()?;
        let context = self
            .approval_contexts
            .lock()
            .expect("computer approval context poisoned")
            .remove(&request.request_id)
            .ok_or_else(|| {
                BrokerError::ApprovalContextChanged(
                    "approval return context is missing or already consumed".into(),
                )
            })?;

        self.emit_approval_event(
            ComputerEventKind::ApprovalResumeStarted,
            &context,
            Some("foreground_transition=approval_resume".into()),
        );

        if request.session != context.session
            || request.execution != context.request
            || policy::risk_for(&request.execution) != context.risk
        {
            return Err(self.approval_context_error(
                &context,
                "approval binding no longer matches the requested action",
            ));
        }

        let session = {
            self.refresh_state();
            let state = self.state.read().expect("computer broker poisoned");
            self.validate_locked(&state, &request.session)?
                .session
                .clone()
        };
        let windows = session.window_list().map_err(BrokerError::from)?;
        let actual_window = windows.iter().find(|window| window.active);
        let Some(target) = context.target_window.as_ref() else {
            return Err(
                self.approval_context_error(&context, "approval did not capture a target window")
            );
        };
        let Some(fresh_target) = windows.iter().find(|window| window.id == target.id) else {
            return Err(self.approval_context_error(
                &context,
                "original target window is no longer available",
            ));
        };
        if !same_target_identity(target, fresh_target, &context.request) {
            return Err(self.approval_context_error(
                &context,
                "original target window or bounds changed during approval",
            ));
        }
        let fresh_profile = session
            .capability_probe(&fresh_target.id)
            .map_err(BrokerError::from)?;
        if let Some(expected) = context.target_application.as_ref() {
            if &fresh_profile.application != expected {
                return Err(self.approval_context_error(
                    &context,
                    "target application identity changed during approval",
                ));
            }
        }
        if context.target_security != fresh_profile.security {
            return Err(self.approval_context_error(
                &context,
                "target security context changed during approval",
            ));
        }
        if context.semantic_generation != fresh_profile.semantic_generation {
            return Err(self
                .approval_context_error(&context, "semantic generation changed during approval"));
        }
        if let Some(element_id) = request.execution.semantic_element_id() {
            session.validate_element(element_id).map_err(|error| {
                self.approval_context_error(&context, &format!("semantic target is stale: {error}"))
            })?;
        }
        if let Some(expected_topology) = context.topology_generation {
            let fresh_observation = session.observe().map_err(BrokerError::from)?;
            if fresh_observation
                .display_topology
                .as_ref()
                .map(|topology| topology.topology_generation)
                != Some(expected_topology)
            {
                return Err(self
                    .approval_context_error(&context, "display topology changed during approval"));
            }
        }

        match foreground_transition(&context, actual_window, &target.id) {
            ForegroundTransition::ExpectedHostTransition => {
                self.emit_approval_event(
                    ComputerEventKind::ApprovalApproved,
                    &context,
                    Some(format!(
                        "foreground_transition={}",
                        ForegroundTransition::ExpectedHostTransition.as_str()
                    )),
                );
                self.restore_approved_target_focus(&session, &context)?;
            }
            ForegroundTransition::ExpectedAgentTransition => {
                self.emit_approval_event(
                    ComputerEventKind::ApprovalApproved,
                    &context,
                    Some(format!(
                        "foreground_transition={}",
                        ForegroundTransition::ExpectedAgentTransition.as_str()
                    )),
                );
            }
            ForegroundTransition::UnexpectedTransition => {
                return Err(self.approval_context_error(
                    &context,
                    &format!(
                        "foreground changed to an unrelated window; transition={}",
                        ForegroundTransition::UnexpectedTransition.as_str()
                    ),
                ));
            }
        }

        // Focus restore is host-owned and is not the requested business
        // action. Establish the monitor baseline after it, then run the full
        // normal admission path with the original target bound explicitly.
        self.interaction
            .release_for_approval(&request.session.computer_session_id);
        let mut policy = self.policy();
        policy.set_profile(profile);
        policy.record_approval(request.request_id.clone());
        let result =
            self.execute_with_policy_bound(request, &policy, Some(target.id.clone()), None);
        match &result {
            Ok(_) => self.emit_approval_event(
                ComputerEventKind::ApprovalResumeCompleted,
                &context,
                Some("resume_validation=passed".into()),
            ),
            Err(BrokerError::TargetForegroundChanged { .. }) => self.emit_approval_event(
                ComputerEventKind::ApprovalResumeContextChanged,
                &context,
                Some("resume_validation=foreground_changed".into()),
            ),
            Err(error) if error.code() == "USER_TAKEOVER" => self.emit_approval_event(
                ComputerEventKind::ApprovalResumeContextChanged,
                &context,
                Some("resume_validation=user_takeover".into()),
            ),
            Err(_) => {}
        }
        self.revoke_approval(&context.approval_id);
        result
    }

    /// Deny consumes the approval context without restoring focus or creating
    /// another request. This is intentionally idempotent for expired UI state.
    pub fn discard_approval_context(&self, request_id: &str) {
        let context = self
            .approval_contexts
            .lock()
            .expect("computer approval context poisoned")
            .remove(request_id);
        if let Some(context) = context {
            self.emit_approval_event(
                ComputerEventKind::ApprovalDenied,
                &context,
                Some("dispatch=0".into()),
            );
        }
        self.revoke_approval(request_id);
    }

    /// Execute with the approval profile resolved from the authoritative
    /// Authoritative host permission context for this request. The override is
    /// request-scoped: protected surfaces, capability admission, foreground
    /// checks, interaction safety, leases, and lifecycle validation remain
    /// unchanged, and the broker's per-request approval grants are retained.
    pub fn execute_with_profile(
        &self,
        request: BrokerExecutionRequest,
        profile: ComputerApprovalProfile,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        let mut policy = self.policy();
        policy.set_profile(profile);
        self.execute_with_policy(request, &policy)
    }

    fn execute_with_policy(
        &self,
        request: BrokerExecutionRequest,
        policy: &ComputerPolicy,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        self.execute_with_policy_bound(request, policy, None, None)
    }

    fn execute_with_policy_bound(
        &self,
        request: BrokerExecutionRequest,
        policy: &ComputerPolicy,
        forced_target: Option<WindowId>,
        batch_context: Option<&BrokerExecutionBatchContext>,
    ) -> Result<ComputerExecutionResult, BrokerError> {
        request.validate()?;
        let action_kind = telemetry::action_kind(&request.execution);
        let started = Instant::now();
        self.emit_event(ComputerEvent {
            event_type: ComputerEventKind::ActionRequested,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(request.session.computer_session_id.to_string()),
            owner: Some(request.session.owner.clone()),
            request_id: Some(request.request_id.clone()),
            action_kind: Some(action_kind.clone()),
            outcome: None,
            detail: None,
            ux_state: None,
            ux_transition: None,
        });
        let _action_gate = self
            .action_gate
            .lock()
            .expect("computer action gate poisoned");
        self.refresh_state();
        let session = {
            let state = self.state.read().expect("computer broker poisoned");
            let session = self
                .validate_locked(&state, &request.session)?
                .session
                .clone();
            self.validate_lease_locked(&state, &request.session, &request.lease)?;
            session
        };
        let interaction_action = interaction_action_class(&request.execution);
        let interaction_target = requested_interaction_target(&request.execution);
        let mut interaction_lease = match self.interaction.begin_lease(
            &request.lease,
            &request.session,
            interaction_action,
            interaction_target,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                self.cleanup_after_interaction_conflict(
                    &session,
                    &request.session.computer_session_id,
                    &error,
                );
                self.emit_event(self.event_for_error(
                    ComputerEventKind::ActionBlocked,
                    &request,
                    Some(error.to_string()),
                    Some(action_kind.clone()),
                ));
                self.audit_error(&request, &action_kind, started.elapsed(), &error);
                return Err(error);
            }
        };
        interaction_lease.target_application = request.execution.target_application().cloned();
        let windows = match batch_context {
            Some(context) => context.windows.clone(),
            None => session.window_list().map_err(BrokerError::from)?,
        };
        let mut target = request_target_window(&request.execution)
            .or_else(|| requested_interaction_target(&request.execution))
            .or(forced_target);
        if target.is_none()
            && !is_focus_window(&request.execution)
            && !is_unscoped_keyboard_input(&request.execution)
        {
            // Pointer/keyboard actions without an explicit target still act
            // on the observed foreground window. Binding that fresh window
            // into admission prevents a protected active surface from being
            // reached through an unscoped coordinate action.
            target = windows
                .iter()
                .find(|window| window.active)
                .map(|window| window.id.clone());
        }
        if interaction_lease.target.is_none() {
            interaction_lease.target = target.clone();
        }
        let target_window = if let Some(context) = batch_context {
            context.target_window.as_ref()
        } else {
            target
                .as_ref()
                .map(|target| {
                    windows
                        .iter()
                        .find(|window| &window.id == target)
                        .ok_or_else(|| {
                            BrokerError::InvalidRequest(
                                "target window is not present in the fresh window list".into(),
                            )
                        })
                })
                .transpose()?
        };
        if let Some(application) = request.execution.target_application() {
            let matches = target_window.is_some_and(|window| {
                application
                    .process_id
                    .is_some_and(|process_id| window.process_id == Some(process_id))
            });
            if !matches {
                return Err(BrokerError::InvalidRequest(
                    "application target does not match the current window context".into(),
                ));
            }
        }
        let mut capability_profile = None;
        // Semantic actions already carry a generation-scoped AX element. A
        // capability probe here is both unnecessary (pixel admission only)
        // and harmful on macOS because AX probing can refresh the semantic
        // snapshot and invalidate that element before dispatch.
        if matches!(
            &request.execution.intent,
            ComputerExecutionIntent::Pixel { .. }
        ) && !is_unscoped_keyboard_input(&request.execution)
        {
            if let Some(target_window) = target_window.as_ref() {
                let profile = if let Some(context) = batch_context {
                    context.capability_profile.clone().ok_or_else(|| {
                        BrokerError::InvalidRequest(
                            "prepared batch context is missing target capability data".into(),
                        )
                    })?
                } else {
                    self.capability_probe(&request.session, &target_window.id)
                        .inspect_err(|error| {
                            self.emit_event(self.event_for_error(
                                ComputerEventKind::ActionBlocked,
                                &request,
                                Some(error.to_string()),
                                Some(action_kind.clone()),
                            ));
                        })?
                };
                admit_pixel_capability(&request.execution, &profile)?;
                capability_profile = Some(profile);
            }
        }
        let admission = policy.admit(
            &request.request_id,
            &request.session.owner,
            &request.execution,
            target_window,
        );
        if let Err(error) = admission {
            let kind = if matches!(error, BrokerError::ApprovalRequired { .. }) {
                if let BrokerError::ApprovalRequired { approval_id, risk } = &error {
                    self.capture_approval_context(
                        &request,
                        target_window,
                        capability_profile.as_ref(),
                        &windows,
                        action_kind.clone(),
                        approval_id,
                        *risk,
                        &session,
                    );
                    // The pre-approval lease is intentionally not carried
                    // across the host approval interaction. The next phase
                    // establishes a fresh monitor baseline.
                    self.interaction
                        .release_for_approval(&request.session.computer_session_id);
                }
                ComputerEventKind::ApprovalRequired
            } else {
                ComputerEventKind::ActionBlocked
            };
            self.emit_event(self.event_for_error(
                kind,
                &request,
                Some(error.to_string()),
                Some(action_kind.clone()),
            ));
            self.audit_error(&request, &action_kind, started.elapsed(), &error);
            return Err(error);
        }
        if let Err(error) = self.interaction.preflight(
            &interaction_lease,
            &windows,
            target.as_ref(),
            is_focus_window(&request.execution),
            background_route_allowed(&request.execution),
        ) {
            self.cleanup_after_interaction_conflict(
                &session,
                &request.session.computer_session_id,
                &error,
            );
            self.emit_event(self.event_for_error(
                ComputerEventKind::ActionBlocked,
                &request,
                Some(error.to_string()),
                Some(action_kind.clone()),
            ));
            self.audit_error(&request, &action_kind, started.elapsed(), &error);
            return Err(error);
        }

        let dispatch_key = (
            request.session.computer_session_id.clone(),
            request.request_id.clone(),
        );
        {
            let mut state = self.state.write().expect("computer broker poisoned");
            self.validate_locked(&state, &request.session)?;
            self.validate_lease_locked(&state, &request.session, &request.lease)?;
            if !state.dispatched.insert(dispatch_key.clone()) {
                return Err(BrokerError::DuplicateRequest(request.request_id));
            }
        }

        // One final foreground/activity admission occurs after exactly-once
        // reservation but before ActionStarted or the backend call. If this
        // check fails, release the reservation: no action was dispatched and
        // the caller may make a fresh, explicitly admitted request.
        let final_windows = match session.window_list() {
            Ok(windows) => windows,
            Err(error) => {
                let error = BrokerError::from(error);
                self.state
                    .write()
                    .expect("computer broker poisoned")
                    .dispatched
                    .remove(&dispatch_key);
                self.emit_event(self.event_for_error(
                    ComputerEventKind::ActionBlocked,
                    &request,
                    Some(error.to_string()),
                    Some(action_kind.clone()),
                ));
                self.audit_error(&request, &action_kind, started.elapsed(), &error);
                return Err(error);
            }
        };
        if let Some(context) = batch_context {
            if let Some(expected) = context.target_window.as_ref() {
                // A global keyboard action deliberately follows the current
                // foreground surface. Transient menus/popovers may disappear
                // while the event is in flight, so the batch's initial window
                // identity must not be treated as a stale target for this
                // unscoped path. Foreground and interaction checks still run.
                if !is_unscoped_keyboard_input(&request.execution) {
                    let identity_ok =
                        if let Some(application) = request.execution.target_application() {
                            final_windows.iter().any(|window| {
                                (background_route_allowed(&request.execution) || window.active)
                                    && application.process_id.is_some_and(|process_id| {
                                        window.process_id == Some(process_id)
                                    })
                            })
                        } else {
                            final_windows
                                .iter()
                                .find(|window| window.id == expected.id)
                                .is_some_and(|actual| {
                                    same_batch_target_identity(expected, actual, &request.execution)
                                })
                        };
                    if !identity_ok {
                        let error = BrokerError::InvalidRequest(
                            "target window identity or bounds changed during the prepared batch"
                                .into(),
                        );
                        self.state
                            .write()
                            .expect("computer broker poisoned")
                            .dispatched
                            .remove(&dispatch_key);
                        self.cleanup_after_interaction_conflict(
                            &session,
                            &request.session.computer_session_id,
                            &error,
                        );
                        self.emit_event(self.event_for_error(
                            ComputerEventKind::ActionBlocked,
                            &request,
                            Some(error.to_string()),
                            Some(action_kind.clone()),
                        ));
                        self.audit_error(&request, &action_kind, started.elapsed(), &error);
                        return Err(error);
                    }
                }
            }
        }
        if let Err(error) = self.interaction.preflight(
            &interaction_lease,
            &final_windows,
            target.as_ref(),
            is_focus_window(&request.execution),
            background_route_allowed(&request.execution),
        ) {
            self.state
                .write()
                .expect("computer broker poisoned")
                .dispatched
                .remove(&dispatch_key);
            self.cleanup_after_interaction_conflict(
                &session,
                &request.session.computer_session_id,
                &error,
            );
            self.emit_event(self.event_for_error(
                ComputerEventKind::ActionBlocked,
                &request,
                Some(error.to_string()),
                Some(action_kind.clone()),
            ));
            self.audit_error(&request, &action_kind, started.elapsed(), &error);
            return Err(error);
        }
        self.emit_event(self.event_for_error(
            ComputerEventKind::ActionStarted,
            &request,
            None,
            Some(action_kind.clone()),
        ));
        let result = session
            .execute_policy(&request.execution)
            .map_err(BrokerError::from);
        let result = match result {
            Ok(value) => {
                let focus_result = if is_focus_window(&request.execution) {
                    let windows =
                        session
                            .window_list()
                            .map_err(|error| BrokerError::OutcomeUnknown {
                                code: "TARGET_FOREGROUND_CHANGED".into(),
                                message: format!(
                                    "foreground could not be verified after FocusWindow: {}",
                                    BrokerError::from(error)
                                ),
                            })?;
                    let actual = foreground_window(&windows);
                    if actual.as_ref() != interaction_lease.target.as_ref() {
                        Err(BrokerError::OutcomeUnknown {
                            code: "TARGET_FOREGROUND_CHANGED".into(),
                            message: format!(
                                "FocusWindow completed without target foreground: target={:?} actual={actual:?}",
                                interaction_lease.target
                            ),
                        })
                    } else {
                        Ok(())
                    }
                } else {
                    Ok(())
                };
                focus_result.and_then(|()| {
                    self.interaction
                        .postflight(&mut interaction_lease)
                        .map(|()| value)
                })
            }
            Err(error) => Err(error),
        };
        match &result {
            Ok(value) if value.final_outcome == ComputerExecutionOutcome::Performed => {
                self.interaction
                    .commit_gesture(&request.session.computer_session_id, &interaction_lease);
            }
            Ok(_) => {
                if interaction_lease.action.begins_held_gesture()
                    || interaction_lease.action.ends_held_gesture()
                {
                    let _ = session.cleanup_pressed();
                    self.interaction
                        .clear_gesture(&request.session.computer_session_id);
                }
            }
            Err(error) => {
                self.cleanup_after_interaction_conflict(
                    &session,
                    &request.session.computer_session_id,
                    error,
                );
            }
        }
        self.interaction.complete();
        match &result {
            Ok(value) => {
                let unknown = telemetry::is_unknown(value);
                self.emit_event(self.event_for_result(
                    if unknown {
                        ComputerEventKind::OutcomeUnknown
                    } else {
                        ComputerEventKind::ActionCompleted
                    },
                    &request,
                    Some(telemetry::outcome_name(value)),
                    Some(action_kind.clone()),
                ));
                self.audit_result(&request, &action_kind, started.elapsed(), value);
            }
            Err(error) => {
                self.emit_event(self.event_for_error(
                    if error.outcome_unknown() {
                        ComputerEventKind::OutcomeUnknown
                    } else {
                        ComputerEventKind::ActionFailed
                    },
                    &request,
                    Some(error.to_string()),
                    Some(action_kind.clone()),
                ));
                self.audit_error(&request, &action_kind, started.elapsed(), error);
            }
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    fn capture_approval_context(
        &self,
        request: &BrokerExecutionRequest,
        target_window: Option<&Window>,
        capability: Option<&ApplicationCapabilityProfile>,
        windows: &[Window],
        action_kind: String,
        approval_id: &str,
        risk: ComputerRiskLevel,
        session: &ComputerHostSession,
    ) {
        let topology_generation = session.observe().ok().and_then(|observation| {
            observation
                .display_topology
                .map(|topology| topology.topology_generation)
        });
        let context = ApprovalReturnContext {
            session: request.session.clone(),
            request: request.execution.clone(),
            target_window: target_window.cloned(),
            target_application: capability.map(|profile| profile.application.clone()),
            target_security: capability.and_then(|profile| profile.security.clone()),
            original_foreground: foreground_window(windows),
            semantic_generation: capability.and_then(|profile| profile.semantic_generation),
            topology_generation,
            action_kind,
            approval_id: approval_id.to_owned(),
            risk,
        };
        self.approval_contexts
            .lock()
            .expect("computer approval context poisoned")
            .insert(approval_id.to_owned(), context);
    }

    fn approval_context_error(&self, context: &ApprovalReturnContext, detail: &str) -> BrokerError {
        let error = if detail.contains("foreground changed to an unrelated window") {
            BrokerError::ApprovalContextChanged(format!("UserContextChanged: {detail}"))
        } else {
            BrokerError::ApprovalContextChanged(detail.to_owned())
        };
        self.emit_approval_event(
            ComputerEventKind::ApprovalResumeContextChanged,
            context,
            Some(error.to_string()),
        );
        self.audit_approval_error(context, &error);
        error
    }

    fn restore_approved_target_focus(
        &self,
        session: &ComputerHostSession,
        context: &ApprovalReturnContext,
    ) -> Result<(), BrokerError> {
        let target = context.target_window.as_ref().ok_or_else(|| {
            BrokerError::ApprovalFocusRestoreFailed("target window is missing".into())
        })?;
        let request = ComputerExecutionRequest::pixel(
            ComputerAction::FocusWindow {
                window_id: target.id.clone(),
            },
            Some(target.id.clone()),
        );
        let result = session.execute_policy(&request).map_err(|error| {
            let error = BrokerError::ApprovalFocusRestoreFailed(error.to_string());
            self.emit_approval_event(
                ComputerEventKind::ApprovalResumeContextChanged,
                context,
                Some(error.to_string()),
            );
            self.audit_approval_error(context, &error);
            error
        })?;
        if result.final_outcome != ComputerExecutionOutcome::Performed {
            let error = BrokerError::ApprovalFocusRestoreFailed(format!(
                "host focus action returned {:?}",
                result.final_outcome
            ));
            self.emit_approval_event(
                ComputerEventKind::ApprovalResumeContextChanged,
                context,
                Some(error.to_string()),
            );
            self.audit_approval_error(context, &error);
            return Err(error);
        }
        let windows = session.window_list().map_err(BrokerError::from)?;
        if foreground_window(&windows).as_ref() != Some(&target.id) {
            let error = BrokerError::ApprovalFocusRestoreFailed(
                "host focus action did not restore the original foreground".into(),
            );
            self.emit_approval_event(
                ComputerEventKind::ApprovalResumeContextChanged,
                context,
                Some(error.to_string()),
            );
            self.audit_approval_error(context, &error);
            return Err(error);
        }
        Ok(())
    }

    fn emit_approval_event(
        &self,
        event_type: ComputerEventKind,
        context: &ApprovalReturnContext,
        detail: Option<String>,
    ) {
        self.emit_event(ComputerEvent {
            event_type,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(context.session.computer_session_id.to_string()),
            owner: Some(context.session.owner.clone()),
            request_id: Some(context.approval_id.clone()),
            action_kind: Some(context.action_kind.clone()),
            outcome: None,
            detail,
            ux_state: Some(self.interaction_ux_state().to_string()),
            ux_transition: None,
        });
    }

    fn audit_approval_error(&self, context: &ApprovalReturnContext, error: &BrokerError) {
        self.audit_sink
            .read()
            .expect("computer event sink poisoned")
            .record(ComputerAuditRecord {
                at_unix_ms: telemetry::now_unix_ms(),
                computer_session_id: context.session.computer_session_id.to_string(),
                owner: context.session.owner.clone(),
                request_id: context.approval_id.clone(),
                action_kind: context.action_kind.clone(),
                target_window_id: context
                    .target_window
                    .as_ref()
                    .map(|window| window.id.to_string()),
                outcome: error.code().to_owned(),
                outcome_unknown: error.outcome_unknown(),
                duration_ms: 0,
                detail: Some(error.to_string()),
            });
    }

    fn with_session<T>(
        &self,
        reference: &BrokerSessionRef,
        operation: impl FnOnce(&ComputerHostSession) -> Result<T, ComputerHostError>,
    ) -> Result<T, BrokerError> {
        self.refresh_state();
        let session = {
            let state = self.state.read().expect("computer broker poisoned");
            self.validate_locked(&state, reference)?.session.clone()
        };
        operation(&session).map_err(BrokerError::from)
    }

    fn cleanup_after_interaction_conflict(
        &self,
        session: &ComputerHostSession,
        session_id: &ComputerSessionId,
        error: &BrokerError,
    ) {
        if error.requires_pressed_cleanup() {
            let _ = session.cleanup_pressed();
            self.interaction.clear_gesture(session_id);
        }
    }

    fn refresh_state(&self) {
        let status = self.host.status();
        let mut state = self.state.write().expect("computer broker poisoned");
        self.sync_locked(&mut state, &status);
    }

    fn sync_locked(&self, state: &mut BrokerState, status: &ComputerHostStatus) {
        if status.state != ComputerHostServiceState::Ready {
            if !state.sessions.is_empty() {
                state.invalidated = true;
                self.emit_event(ComputerEvent {
                    event_type: ComputerEventKind::SessionInvalidated,
                    at_unix_ms: telemetry::now_unix_ms(),
                    computer_session_id: None,
                    owner: None,
                    request_id: None,
                    action_kind: None,
                    outcome: None,
                    detail: Some(format!("host state={:?}", status.state)),
                    ux_state: None,
                    ux_transition: None,
                });
            }
            state.sessions.clear();
            state.lease = None;
            state.dispatched.clear();
            self.clear_all_approval_contexts();
            return;
        }
        if state.generation != 0 && state.generation != status.generation {
            state.sessions.clear();
            state.dispatched.clear();
            self.clear_all_approval_contexts();
            state.invalidated = true;
        }
        state.generation = status.generation;
    }

    fn clear_approval_contexts_for_session(&self, session_id: &ComputerSessionId) {
        let approval_ids = {
            let mut contexts = self
                .approval_contexts
                .lock()
                .expect("computer approval context poisoned");
            let approval_ids = contexts
                .iter()
                .filter(|(_, context)| &context.session.computer_session_id == session_id)
                .map(|(approval_id, _)| approval_id.clone())
                .collect::<Vec<_>>();
            contexts.retain(|_, context| &context.session.computer_session_id != session_id);
            approval_ids
        };
        for approval_id in approval_ids {
            self.revoke_approval(&approval_id);
        }
    }

    fn clear_all_approval_contexts(&self) {
        let approval_ids = self
            .approval_contexts
            .lock()
            .expect("computer approval context poisoned")
            .drain()
            .map(|(approval_id, _)| approval_id)
            .collect::<Vec<_>>();
        for approval_id in approval_ids {
            self.revoke_approval(&approval_id);
        }
    }

    fn emit_event(&self, event: ComputerEvent) {
        self.event_sink
            .read()
            .expect("computer event sink poisoned")
            .emit(event);
    }

    fn event_for_error(
        &self,
        event_type: ComputerEventKind,
        request: &BrokerExecutionRequest,
        detail: Option<String>,
        action_kind: Option<String>,
    ) -> ComputerEvent {
        ComputerEvent {
            event_type,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(request.session.computer_session_id.to_string()),
            owner: Some(request.session.owner.clone()),
            request_id: Some(request.request_id.clone()),
            action_kind,
            outcome: None,
            detail,
            ux_state: Some(self.interaction_ux_state().to_string()),
            ux_transition: self
                .interaction
                .state()
                .ux_transition()
                .map(|state| state.to_string()),
        }
    }

    fn event_for_result(
        &self,
        event_type: ComputerEventKind,
        request: &BrokerExecutionRequest,
        outcome: Option<String>,
        action_kind: Option<String>,
    ) -> ComputerEvent {
        ComputerEvent {
            event_type,
            at_unix_ms: telemetry::now_unix_ms(),
            computer_session_id: Some(request.session.computer_session_id.to_string()),
            owner: Some(request.session.owner.clone()),
            request_id: Some(request.request_id.clone()),
            action_kind,
            outcome,
            detail: None,
            ux_state: Some(self.interaction_ux_state().to_string()),
            ux_transition: None,
        }
    }

    fn audit_result(
        &self,
        request: &BrokerExecutionRequest,
        action_kind: &str,
        duration: Duration,
        result: &ComputerExecutionResult,
    ) {
        self.audit_sink
            .read()
            .expect("computer audit sink poisoned")
            .record(ComputerAuditRecord {
                at_unix_ms: telemetry::now_unix_ms(),
                computer_session_id: request.session.computer_session_id.to_string(),
                owner: request.session.owner.clone(),
                request_id: request.request_id.clone(),
                action_kind: action_kind.to_owned(),
                target_window_id: telemetry::target_window_id(&request.execution),
                outcome: telemetry::outcome_name(result),
                outcome_unknown: telemetry::is_unknown(result),
                duration_ms: duration.as_millis(),
                detail: None,
            });
    }

    fn audit_error(
        &self,
        request: &BrokerExecutionRequest,
        action_kind: &str,
        duration: Duration,
        error: &BrokerError,
    ) {
        self.audit_sink
            .read()
            .expect("computer audit sink poisoned")
            .record(ComputerAuditRecord {
                at_unix_ms: telemetry::now_unix_ms(),
                computer_session_id: request.session.computer_session_id.to_string(),
                owner: request.session.owner.clone(),
                request_id: request.request_id.clone(),
                action_kind: action_kind.to_owned(),
                target_window_id: telemetry::target_window_id(&request.execution),
                outcome: error.code().to_owned(),
                outcome_unknown: error.outcome_unknown(),
                duration_ms: duration.as_millis(),
                detail: Some(error.to_string()),
            });
    }

    fn validate_lease_locked(
        &self,
        state: &BrokerState,
        reference: &BrokerSessionRef,
        lease: &DesktopLeaseRef,
    ) -> Result<(), BrokerError> {
        let Some(current) = state.lease.as_ref() else {
            return Err(BrokerError::LeaseRequired);
        };
        if lease.generation != reference.generation {
            return Err(BrokerError::StaleLease);
        }
        if lease.computer_session_id != reference.computer_session_id
            || lease.owner != reference.owner
        {
            return Err(BrokerError::InvalidLease);
        }
        if lease.expires_at_unix_ms <= telemetry::now_unix_ms() {
            return Err(BrokerError::StaleLease);
        }
        if current != lease {
            return Err(BrokerError::InvalidLease);
        }
        Ok(())
    }

    fn validate_locked<'a>(
        &self,
        state: &'a BrokerState,
        reference: &BrokerSessionRef,
    ) -> Result<&'a BrokerSessionRecord, BrokerError> {
        if reference.generation != state.generation {
            return Err(BrokerError::StaleReference {
                expected_generation: state.generation,
                actual_generation: reference.generation,
            });
        }
        let Some(record) = state.sessions.get(&reference.computer_session_id) else {
            return if state.invalidated {
                Err(BrokerError::StaleReference {
                    expected_generation: state.generation,
                    actual_generation: reference.generation,
                })
            } else {
                Err(BrokerError::SessionNotFound(
                    reference.computer_session_id.clone(),
                ))
            };
        };
        if record.reference.owner != reference.owner {
            return Err(BrokerError::CrossSessionReference);
        }
        Ok(record)
    }
}

fn request_target_window(request: &ComputerExecutionRequest) -> Option<WindowId> {
    match &request.intent {
        ComputerExecutionIntent::Pixel {
            action,
            target_window_id,
            target_application,
        } => target_window_id.clone().or_else(|| match action {
            _ if target_application.is_some() => None,
            ComputerAction::TypeText { target, .. }
            | ComputerAction::KeyPress { target, .. }
            | ComputerAction::Hotkey { target, .. }
            | ComputerAction::MouseDown { target, .. }
            | ComputerAction::MouseUp { target, .. }
            | ComputerAction::MiddleClick { target, .. }
            | ComputerAction::TripleClick { target, .. }
            | ComputerAction::ModifierClick { target, .. }
            | ComputerAction::KeyDown { target, .. }
            | ComputerAction::KeyUp { target, .. }
            | ComputerAction::HoldKey { target, .. } => target.clone(),
            ComputerAction::Click { .. }
            | ComputerAction::DoubleClick { .. }
            | ComputerAction::RightClick { .. }
            | ComputerAction::MovePointer { .. }
            | ComputerAction::Drag { .. }
            | ComputerAction::Scroll { .. }
            | ComputerAction::FocusWindow { .. }
            | ComputerAction::ModifiedPointer { .. } => None,
        }),
        ComputerExecutionIntent::Semantic(_) => None,
    }
}

fn requested_interaction_target(request: &ComputerExecutionRequest) -> Option<WindowId> {
    request_target_window(request).or_else(|| match &request.intent {
        ComputerExecutionIntent::Pixel {
            action: ComputerAction::FocusWindow { window_id },
            ..
        } => Some(window_id.clone()),
        _ => None,
    })
}

fn is_global_desktop_shortcut(request: &ComputerExecutionRequest) -> bool {
    let ComputerExecutionIntent::Pixel {
        action: ComputerAction::Hotkey {
            keys, target: None, ..
        },
        target_window_id: None,
        target_application: None,
    } = &request.intent
    else {
        return false;
    };
    let has = |names: &[&str]| keys.iter().any(|key| names.contains(&key.as_str()));
    (has(&["meta", "cmd", "command"]) && has(&["tab", "space", "w", "q"]))
        || (has(&["alt", "option"]) && has(&["tab"]))
}

fn is_unscoped_keyboard_input(request: &ComputerExecutionRequest) -> bool {
    if is_global_desktop_shortcut(request) {
        return true;
    }
    let ComputerExecutionIntent::Pixel {
        action,
        target_window_id: None,
        target_application: None,
    } = &request.intent
    else {
        return false;
    };
    matches!(
        action,
        ComputerAction::TypeText { target: None, .. }
            | ComputerAction::KeyPress { target: None, .. }
            | ComputerAction::Hotkey { target: None, .. }
            | ComputerAction::KeyDown { target: None, .. }
            | ComputerAction::KeyUp { target: None, .. }
            | ComputerAction::HoldKey { target: None, .. }
    )
}

/// Background admission is currently a macOS-native capability. Keeping this
/// gate in the broker prevents the portable default from weakening Windows'
/// foreground contract before a Windows background provider is implemented.
fn background_route_allowed(request: &ComputerExecutionRequest) -> bool {
    cfg!(target_os = "macos") && request.allows_background()
}

fn interaction_action_class(request: &ComputerExecutionRequest) -> InteractionActionClass {
    match &request.intent {
        ComputerExecutionIntent::Semantic(_) => InteractionActionClass::Semantic,
        ComputerExecutionIntent::Pixel { action, .. } => match action {
            ComputerAction::TypeText { at: Some(_), .. } => InteractionActionClass::SpatialTypeText,
            ComputerAction::TypeText { .. } => InteractionActionClass::TypeText,
            ComputerAction::KeyPress { .. }
            | ComputerAction::Hotkey { .. }
            | ComputerAction::HoldKey { .. } => InteractionActionClass::Key,
            ComputerAction::KeyDown { .. } => InteractionActionClass::HeldKeyDown,
            ComputerAction::KeyUp { .. } => InteractionActionClass::HeldKeyUp,
            ComputerAction::Click { .. }
            | ComputerAction::DoubleClick { .. }
            | ComputerAction::RightClick { .. }
            | ComputerAction::MiddleClick { .. }
            | ComputerAction::TripleClick { .. }
            | ComputerAction::ModifierClick { .. } => InteractionActionClass::Click,
            ComputerAction::MovePointer { .. }
            | ComputerAction::Drag { .. }
            | ComputerAction::Scroll { .. } => InteractionActionClass::PointerGesture,
            ComputerAction::MouseDown { .. } => InteractionActionClass::HeldPointerDown,
            ComputerAction::MouseUp { .. } => InteractionActionClass::HeldPointerUp,
            ComputerAction::FocusWindow { .. } => InteractionActionClass::FocusWindow,
            ComputerAction::ModifiedPointer { action, .. } => match action {
                alice_computer_use_core::ComputerPointerAction::Click { .. } => {
                    InteractionActionClass::Click
                }
                alice_computer_use_core::ComputerPointerAction::Move { .. }
                | alice_computer_use_core::ComputerPointerAction::Drag { .. }
                | alice_computer_use_core::ComputerPointerAction::Scroll { .. } => {
                    InteractionActionClass::PointerGesture
                }
            },
        },
    }
}

fn is_focus_window(request: &ComputerExecutionRequest) -> bool {
    matches!(
        request.intent,
        ComputerExecutionIntent::Pixel {
            action: ComputerAction::FocusWindow { .. },
            ..
        }
    )
}

fn admit_pixel_capability(
    request: &ComputerExecutionRequest,
    profile: &ApplicationCapabilityProfile,
) -> Result<(), BrokerError> {
    let ComputerExecutionIntent::Pixel { action, .. } = &request.intent else {
        return Ok(());
    };
    let admit = |assessment: &CapabilityAssessment| match assessment.status {
        CapabilityStatus::Supported => Ok(()),
        CapabilityStatus::Unsupported | CapabilityStatus::Restricted => {
            Err(BrokerError::CapabilityDenied(
                assessment
                    .provenance
                    .detail
                    .clone()
                    .unwrap_or_else(|| format!("status={:?}", assessment.status)),
            ))
        }
        CapabilityStatus::Unknown => Err(BrokerError::CapabilityUnknown(
            assessment
                .provenance
                .detail
                .clone()
                .unwrap_or_else(|| "capability probe returned unknown".into()),
        )),
    };
    if matches!(action, ComputerAction::TypeText { at: Some(_), .. }) {
        admit(&profile.input.pointer)?;
    }
    let assessment = match action {
        ComputerAction::TypeText { .. } => &profile.input.text,
        ComputerAction::KeyPress { .. }
        | ComputerAction::Hotkey { .. }
        | ComputerAction::KeyDown { .. }
        | ComputerAction::KeyUp { .. }
        | ComputerAction::HoldKey { .. } => &profile.input.keyboard,
        ComputerAction::FocusWindow { .. } => &profile.observation.window,
        _ => &profile.input.pointer,
    };
    admit(assessment)
}

impl Drop for ComputerExecutionBroker {
    fn drop(&mut self) {
        if Arc::strong_count(&self.state) == 1 {
            let _ = self.shutdown();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        foreground_transition, same_target_identity, ApprovalReturnContext, BrokerError,
        BrokerSessionRef, ComputerInteractionGuard, ComputerRequestOwner, ComputerSessionId,
        ComputerUxState, DesktopLeaseRef, ForegroundTransition, InteractionActionClass,
        InteractionState,
    };
    use crate::input_activity::{InputActivityMonitor, InputEventKind, InputSource};
    use alice_computer_use_core::{
        ApplicationIdentity, ComputerAction, ComputerExecutionRequest, Coordinate, CoordinateSpace,
        DpiScale, ElementId, Point, ProcessArchitecture, SemanticAction, Size, Window, WindowId,
    };
    use std::sync::Arc;

    #[test]
    fn spatial_text_holds_pointer_ownership_for_the_atomic_action() {
        assert!(InteractionActionClass::SpatialTypeText.blocks_hardware_mouse_move());
        assert!(!InteractionActionClass::TypeText.blocks_hardware_mouse_move());
    }

    #[test]
    fn application_scoped_keyboard_survives_a_top_level_window_change() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(monitor);
        let session = session();
        let mut lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::TypeText,
                Some(WindowId::new("old-window")),
            )
            .unwrap();
        lease.target_application = Some(ApplicationIdentity {
            process_id: Some(42),
            executable_name: Some("Browser".into()),
            executable_path: None,
            executable_hash: None,
            process_architecture: ProcessArchitecture::Unknown,
            top_level_window_class: None,
            framework_hints: Vec::new(),
            version: None,
        });
        let mut active = window("new-tab", true);
        active.process_id = Some(42);
        assert!(guard
            .preflight(
                &lease,
                &[active],
                Some(&WindowId::new("old-window")),
                false,
                true,
            )
            .is_ok());
    }

    #[test]
    fn global_desktop_shortcut_is_not_bound_to_a_window() {
        let request = ComputerExecutionRequest::pixel(
            ComputerAction::Hotkey {
                keys: vec!["meta".into(), "space".into()],
                target: None,
            },
            None,
        );
        assert!(super::is_global_desktop_shortcut(&request));
    }

    fn session() -> BrokerSessionRef {
        BrokerSessionRef {
            computer_session_id: ComputerSessionId::new("test-session"),
            generation: 1,
            owner: ComputerRequestOwner::new("alice", "thread", "turn", "tool", None),
        }
    }

    fn desktop_lease(session: &BrokerSessionRef) -> DesktopLeaseRef {
        DesktopLeaseRef {
            lease_id: "test-lease".into(),
            generation: session.generation,
            computer_session_id: session.computer_session_id.clone(),
            owner: session.owner.clone(),
            expires_at_unix_ms: u64::MAX,
        }
    }

    fn window(id: &str, active: bool) -> Window {
        Window {
            id: WindowId::new(id),
            title: id.to_owned(),
            class_name: None,
            process_id: None,
            bounds: Coordinate {
                space: CoordinateSpace::DesktopPhysical,
                point: Point { x: 0.0, y: 0.0 },
                extent: Size {
                    width: 100.0,
                    height: 100.0,
                },
                dpi: DpiScale::ONE,
                display_id: None,
                frame_id: None,
            },
            screen_id: None,
            active,
            security: None,
        }
    }

    #[test]
    fn observation_establishes_agent_input_baseline() {
        let guard = ComputerInteractionGuard::new(Arc::new(InputActivityMonitor::for_test(true)));
        guard.observe();
        assert_eq!(guard.state(), InteractionState::NoConflict);
    }

    #[test]
    fn hardware_keyboard_revokes_key_lease() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Key,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::Hardware, InputEventKind::Keyboard);
        assert!(matches!(
            guard.check_activity(&lease, false),
            Err(BrokerError::UserTakeover)
        ));
        assert_eq!(guard.state(), InteractionState::UserTakeover);
    }

    #[test]
    fn physical_mouse_move_does_not_revoke_text_lease() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::TypeText,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::Hardware, InputEventKind::MouseMove);
        assert!(guard.check_activity(&lease, false).is_ok());
    }

    #[test]
    fn physical_mouse_move_does_not_revoke_click_lease() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Click,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::Hardware, InputEventKind::MouseMove);
        assert!(guard.check_activity(&lease, false).is_ok());
    }

    #[test]
    fn other_injected_mouse_move_does_not_revoke_click_lease() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Click,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::OtherInjected, InputEventKind::MouseMove);
        assert!(guard.check_activity(&lease, false).is_ok());
    }

    #[test]
    fn click_lease_revokes_on_button_keyboard_or_wheel() {
        for kind in [
            InputEventKind::MouseButton,
            InputEventKind::Keyboard,
            InputEventKind::MouseWheel,
        ] {
            let monitor = Arc::new(InputActivityMonitor::for_test(true));
            let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
            let session = session();
            let lease = guard
                .begin_lease(
                    &desktop_lease(&session),
                    &session,
                    InteractionActionClass::Click,
                    None,
                )
                .unwrap();
            monitor.record_for_test(InputSource::Hardware, kind);
            assert!(matches!(
                guard.check_activity(&lease, false),
                Err(BrokerError::UserTakeover)
            ));
        }
    }

    #[test]
    fn physical_mouse_move_revokes_pointer_gesture() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::PointerGesture,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::Hardware, InputEventKind::MouseMove);
        assert!(matches!(
            guard.check_activity(&lease, false),
            Err(BrokerError::UserTakeover)
        ));
    }

    #[test]
    fn other_injected_mouse_move_revokes_pointer_gesture() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::PointerGesture,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::OtherInjected, InputEventKind::MouseMove);
        assert!(matches!(
            guard.check_activity(&lease, false),
            Err(BrokerError::ExternalInputConflict)
        ));
    }

    #[test]
    fn foreground_change_is_context_changed_not_user_takeover() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(monitor);
        let session = session();
        let target = WindowId::new("target");
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Click,
                Some(target.clone()),
            )
            .unwrap();
        assert!(matches!(
            guard.preflight(
                &lease,
                &[window("other", true)],
                Some(&target),
                false,
                false
            ),
            Err(BrokerError::TargetForegroundChanged { .. })
        ));
        assert_eq!(guard.state(), InteractionState::ContextChanged);
        assert_eq!(guard.state().ux_state(), ComputerUxState::ContextChanged);
    }

    #[test]
    fn held_pointer_baseline_carries_into_mouse_up() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let down = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::HeldPointerDown,
                None,
            )
            .unwrap();
        guard.check_activity(&down, false).unwrap();
        guard.commit_gesture(&session.computer_session_id, &down);
        monitor.record_for_test(InputSource::Hardware, InputEventKind::MouseMove);
        let up = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::HeldPointerUp,
                None,
            )
            .unwrap();
        assert!(matches!(
            guard.check_activity(&up, false),
            Err(BrokerError::UserTakeover)
        ));
    }

    #[test]
    fn user_takeover_maps_to_paused_by_user() {
        assert_eq!(
            InteractionState::UserTakeover.ux_state(),
            ComputerUxState::PausedByUser
        );
    }

    #[test]
    fn approval_foreground_transitions_distinguish_host_target_and_external() {
        let target = window("target", true);
        let context = ApprovalReturnContext {
            session: session(),
            request: ComputerExecutionRequest::semantic(SemanticAction::Focus {
                element_id: ElementId::new("element"),
            }),
            target_window: Some(target.clone()),
            target_application: None,
            target_security: None,
            original_foreground: Some(target.id.clone()),
            semantic_generation: None,
            topology_generation: None,
            action_kind: "semantic_focus".into(),
            approval_id: "approval".into(),
            risk: crate::policy::ComputerRiskLevel::BenignInteraction,
        };
        let mut host = window("alice", true);
        host.process_id = Some(std::process::id());
        assert_eq!(
            foreground_transition(&context, Some(&host), &target.id),
            ForegroundTransition::ExpectedHostTransition
        );
        assert_eq!(
            foreground_transition(&context, Some(&target), &target.id),
            ForegroundTransition::ExpectedHostTransition
        );
        let external = window("external", true);
        assert_eq!(
            foreground_transition(&context, Some(&external), &target.id),
            ForegroundTransition::UnexpectedTransition
        );
    }

    #[test]
    fn approval_revalidation_rejects_moved_pixel_target_but_keeps_semantic_identity() {
        let expected = window("target", true);
        let mut moved = expected.clone();
        moved.bounds.point.x = 12.0;
        let pixel = ComputerExecutionRequest::pixel(
            ComputerAction::Click {
                at: expected.bounds.clone(),
            },
            Some(expected.id.clone()),
        );
        assert!(!same_target_identity(&expected, &moved, &pixel));
        let semantic = ComputerExecutionRequest::semantic(SemanticAction::Focus {
            element_id: ElementId::new("element"),
        });
        assert!(same_target_identity(&expected, &moved, &semantic));
    }

    #[test]
    fn alice_injection_is_not_a_takeover() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Key,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::AliceInjected, InputEventKind::Keyboard);
        assert!(guard.check_activity(&lease, false).is_ok());
    }

    #[test]
    fn other_injection_is_an_external_conflict() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Key,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::OtherInjected, InputEventKind::Keyboard);
        assert!(matches!(
            guard.check_activity(&lease, false),
            Err(BrokerError::ExternalInputConflict)
        ));
        assert_eq!(guard.state(), InteractionState::ExternalInputConflict);
    }

    #[test]
    fn interrupted_conflict_rearms_the_interaction_guard() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Click,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::OtherInjected, InputEventKind::MouseButton);
        assert!(matches!(
            guard.check_activity(&lease, false),
            Err(BrokerError::ExternalInputConflict)
        ));

        guard.reset_after_interrupt();

        assert_eq!(guard.state(), InteractionState::NoConflict);
        let fresh_lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Click,
                None,
            )
            .unwrap();
        assert!(guard.check_activity(&fresh_lease, false).is_ok());
    }

    #[test]
    fn hardware_after_dispatch_is_outcome_unknown_without_replay() {
        let monitor = Arc::new(InputActivityMonitor::for_test(true));
        let guard = ComputerInteractionGuard::new(Arc::clone(&monitor));
        let session = session();
        let mut lease = guard
            .begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::Key,
                None,
            )
            .unwrap();
        monitor.record_for_test(InputSource::Hardware, InputEventKind::Keyboard);
        assert!(matches!(
            guard.postflight(&mut lease),
            Err(BrokerError::OutcomeUnknown { code, .. }) if code == "USER_TAKEOVER"
        ));
        assert_eq!(guard.state(), InteractionState::UserTakeover);
    }

    #[test]
    fn unavailable_monitor_fails_side_effect_admission_closed() {
        let monitor = Arc::new(InputActivityMonitor::for_test(false));
        let guard = ComputerInteractionGuard::new(monitor);
        let session = session();
        assert!(matches!(
            guard.begin_lease(
                &desktop_lease(&session),
                &session,
                InteractionActionClass::TypeText,
                None,
            ),
            Err(BrokerError::UserActivityUnknown(_))
        ));
        assert_eq!(guard.state(), InteractionState::ActivityUnknown);
    }
}
