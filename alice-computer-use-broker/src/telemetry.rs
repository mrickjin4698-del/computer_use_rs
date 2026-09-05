use crate::ComputerRequestOwner;
use alice_computer_use_core::{
    ComputerAction, ComputerExecutionIntent, ComputerExecutionOutcome, ComputerExecutionRequest,
    ComputerExecutionResult,
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerEventKind {
    SessionStarted,
    ObservationCompleted,
    ActionRequested,
    ActionStarted,
    ActionCompleted,
    ActionFailed,
    ActionBlocked,
    OutcomeUnknown,
    ApprovalRequired,
    ApprovalApproved,
    ApprovalDenied,
    ApprovalResumeStarted,
    ApprovalResumeCompleted,
    ApprovalResumeContextChanged,
    SessionInvalidated,
    SidecarRestarted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerEvent {
    pub event_type: ComputerEventKind,
    pub at_unix_ms: u64,
    pub computer_session_id: Option<String>,
    pub owner: Option<ComputerRequestOwner>,
    pub request_id: Option<String>,
    pub action_kind: Option<String>,
    pub outcome: Option<String>,
    pub detail: Option<String>,
    /// Runtime UX projection.  This is intentionally separate from `outcome`
    /// so a takeover is not presented as a generic tool failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ux_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ux_transition: Option<String>,
}

pub trait ComputerEventSink: Send + Sync {
    fn emit(&self, event: ComputerEvent);
}

pub trait ComputerAuditSink: Send + Sync {
    fn record(&self, record: ComputerAuditRecord);
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerEvidenceMode {
    Disabled,
    Explicit,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerEvidenceMetadata {
    pub evidence_id: String,
    pub phase: String,
    pub byte_len: usize,
    pub captured_at_unix_ms: u64,
}

#[derive(Clone, Debug)]
struct StoredEvidence {
    metadata: ComputerEvidenceMetadata,
    bytes: Vec<u8>,
}

#[derive(Clone)]
pub struct ComputerEvidenceStore {
    inner: Arc<Mutex<EvidenceState>>,
}

#[derive(Debug)]
struct EvidenceState {
    mode: ComputerEvidenceMode,
    max_frames: usize,
    max_bytes: usize,
    ttl: Duration,
    next_id: u64,
    frames: Vec<StoredEvidence>,
}

impl Default for ComputerEvidenceStore {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(EvidenceState {
                mode: ComputerEvidenceMode::Disabled,
                max_frames: 8,
                max_bytes: 16 * 1024 * 1024,
                ttl: Duration::from_secs(300),
                next_id: 1,
                frames: Vec::new(),
            })),
        }
    }
}

impl ComputerEvidenceStore {
    pub fn configure(
        &self,
        mode: ComputerEvidenceMode,
        max_frames: usize,
        max_bytes: usize,
        ttl: Duration,
    ) {
        let mut state = self.inner.lock().expect("computer evidence store poisoned");
        state.mode = mode;
        state.max_frames = max_frames.clamp(1, 32);
        state.max_bytes = max_bytes.clamp(1024, 64 * 1024 * 1024);
        state.ttl = ttl.min(Duration::from_secs(900));
        state.frames.clear();
    }

    pub fn record(
        &self,
        phase: impl Into<String>,
        bytes: Vec<u8>,
    ) -> Option<ComputerEvidenceMetadata> {
        let mut state = self.inner.lock().expect("computer evidence store poisoned");
        if state.mode == ComputerEvidenceMode::Disabled || bytes.len() > state.max_bytes {
            return None;
        }
        let now = now_unix_ms();
        let cutoff = now.saturating_sub(state.ttl.as_millis().min(u64::MAX as u128) as u64);
        state
            .frames
            .retain(|frame| frame.metadata.captured_at_unix_ms >= cutoff);
        while state.frames.len() >= state.max_frames {
            state.frames.remove(0);
        }
        let metadata = ComputerEvidenceMetadata {
            evidence_id: format!("computer-evidence-{}", state.next_id),
            phase: phase.into(),
            byte_len: bytes.len(),
            captured_at_unix_ms: now,
        };
        state.next_id = state.next_id.saturating_add(1);
        state.frames.push(StoredEvidence {
            metadata: metadata.clone(),
            bytes,
        });
        Some(metadata)
    }

    pub fn metadata(&self) -> Vec<ComputerEvidenceMetadata> {
        let mut state = self.inner.lock().expect("computer evidence store poisoned");
        let cutoff =
            now_unix_ms().saturating_sub(state.ttl.as_millis().min(u64::MAX as u128) as u64);
        state
            .frames
            .retain(|frame| frame.metadata.captured_at_unix_ms >= cutoff);
        state
            .frames
            .iter()
            .map(|frame| frame.metadata.clone())
            .collect()
    }

    pub fn clear(&self) {
        self.inner
            .lock()
            .expect("computer evidence store poisoned")
            .frames
            .clear();
    }

    pub fn bytes(&self, evidence_id: &str) -> Option<Vec<u8>> {
        self.inner
            .lock()
            .expect("computer evidence store poisoned")
            .frames
            .iter()
            .find(|frame| frame.metadata.evidence_id == evidence_id)
            .map(|frame| frame.bytes.clone())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerAuditRecord {
    pub at_unix_ms: u64,
    pub computer_session_id: String,
    pub owner: ComputerRequestOwner,
    pub request_id: String,
    pub action_kind: String,
    pub target_window_id: Option<String>,
    pub outcome: String,
    pub outcome_unknown: bool,
    pub duration_ms: u128,
    pub detail: Option<String>,
}

#[derive(Clone, Default)]
pub struct NullComputerEventSink;
impl ComputerEventSink for NullComputerEventSink {
    fn emit(&self, _event: ComputerEvent) {}
}

#[derive(Clone, Default)]
pub struct NullComputerAuditSink;
impl ComputerAuditSink for NullComputerAuditSink {
    fn record(&self, _record: ComputerAuditRecord) {}
}

#[derive(Clone, Default)]
pub struct InMemoryComputerEventSink {
    events: Arc<Mutex<Vec<ComputerEvent>>>,
}

impl InMemoryComputerEventSink {
    pub fn events(&self) -> Vec<ComputerEvent> {
        self.events
            .lock()
            .expect("computer event sink poisoned")
            .clone()
    }
}

impl ComputerEventSink for InMemoryComputerEventSink {
    fn emit(&self, event: ComputerEvent) {
        self.events
            .lock()
            .expect("computer event sink poisoned")
            .push(event);
    }
}

#[derive(Clone, Default)]
pub struct InMemoryComputerAuditSink {
    records: Arc<Mutex<Vec<ComputerAuditRecord>>>,
}

impl InMemoryComputerAuditSink {
    pub fn records(&self) -> Vec<ComputerAuditRecord> {
        self.records
            .lock()
            .expect("computer audit sink poisoned")
            .clone()
    }
}

impl ComputerAuditSink for InMemoryComputerAuditSink {
    fn record(&self, record: ComputerAuditRecord) {
        self.records
            .lock()
            .expect("computer audit sink poisoned")
            .push(record);
    }
}

pub fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0)
}

pub fn action_kind(request: &ComputerExecutionRequest) -> String {
    match &request.intent {
        ComputerExecutionIntent::Semantic(action) => format!("semantic.{action:?}").to_lowercase(),
        ComputerExecutionIntent::Pixel { action, .. } => match action {
            ComputerAction::Click { .. } => "pixel.click",
            ComputerAction::DoubleClick { .. } => "pixel.double_click",
            ComputerAction::RightClick { .. } => "pixel.right_click",
            ComputerAction::MovePointer { .. } => "pixel.move_pointer",
            ComputerAction::Drag { .. } => "pixel.drag",
            ComputerAction::Scroll { .. } => "pixel.scroll",
            ComputerAction::TypeText { .. } => "pixel.type_text",
            ComputerAction::KeyPress { .. } => "pixel.key_press",
            ComputerAction::Hotkey { .. } => "pixel.hotkey",
            ComputerAction::FocusWindow { .. } => "pixel.focus_window",
            ComputerAction::ModifiedPointer { .. } => "pixel.modified_pointer",
            _ => "pixel.input",
        }
        .to_owned(),
    }
    .to_owned()
}

pub fn target_window_id(request: &ComputerExecutionRequest) -> Option<String> {
    match &request.intent {
        ComputerExecutionIntent::Pixel {
            target_window_id, ..
        } => target_window_id.as_ref().map(ToString::to_string),
        ComputerExecutionIntent::Semantic(_) => None,
    }
}

pub fn outcome_name(result: &ComputerExecutionResult) -> String {
    format!("{:?}", result.final_outcome).to_lowercase()
}

pub fn is_unknown(result: &ComputerExecutionResult) -> bool {
    result.final_outcome == ComputerExecutionOutcome::OutcomeUnknown
}
