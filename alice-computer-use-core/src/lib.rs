//! Backend-neutral Computer Use contract.
//!
//! This crate intentionally contains no application UI, agent SDK, runtime-event, or
//! platform automation types.  A coordinate is never just an `(x, y)` pair:
//! it carries its coordinate space, extent, and DPI metadata at the boundary.

use serde::{Deserialize, Serialize};
use std::{fmt, time::SystemTime};

macro_rules! opaque_id {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

opaque_id!(ComputerSessionId);
opaque_id!(WindowId);
opaque_id!(DisplayId);
opaque_id!(FrameId);
opaque_id!(ElementId);

/// Process-lifetime marker used only to attribute Alice's own Win32
/// `SendInput` events in the host input monitor. It is not a security
/// boundary and must never be used as one.
pub const ALICE_COMPUTER_INPUT_MARKER: u64 = 0xA11C_EC0D_5AFE_2026;

/// Backward-compatible name retained for the pre-R6 runtime API.  A screen
/// id is now an opaque display id; no platform monitor handle is exposed.
pub type ScreenId = DisplayId;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DpiScale {
    pub x: f64,
    pub y: f64,
}

impl DpiScale {
    pub const ONE: Self = Self { x: 1.0, y: 1.0 };
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Size {
    pub width: f64,
    pub height: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub origin: Point,
    pub size: Size,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateSpace {
    DesktopPhysical,
    DisplayPhysical,
    DisplayLogical,
    ScreenshotPixel,
    /// R0-R5 wire compatibility. New producers must use an explicit R6
    /// space; WinNative normalizes this value to the primary display.
    Screen,
    Window,
}

/// A point plus the coordinate-space metadata required to interpret it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Coordinate {
    pub space: CoordinateSpace,
    pub point: Point,
    pub extent: Size,
    pub dpi: DpiScale,
    /// Required for display-bound spaces; absent for DesktopPhysical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_id: Option<DisplayId>,
    /// Required for ScreenshotPixel coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame_id: Option<FrameId>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: DisplayId,
    pub name: Option<String>,
    pub physical_bounds: Rect,
    pub work_area: Rect,
    pub physical_size: Size,
    pub logical_size: Size,
    pub dpi: DpiScale,
    pub scale: DpiScale,
    pub primary: bool,
}

/// Compatibility alias for the R0-R5 screen-facing API.
pub type Screen = DisplayInfo;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayTopology {
    pub displays: Vec<DisplayInfo>,
    pub primary_display_id: Option<DisplayId>,
    pub virtual_desktop_bounds: Rect,
    pub topology_generation: u64,
}

impl DisplayTopology {
    pub fn from_displays(displays: Vec<DisplayInfo>, topology_generation: u64) -> Self {
        let primary_display_id = displays
            .iter()
            .find(|display| display.primary)
            .map(|display| display.id.clone());
        let virtual_desktop_bounds = union_rects(
            displays
                .iter()
                .map(|display| display.physical_bounds)
                .collect::<Vec<_>>(),
        )
        .unwrap_or(Rect {
            origin: Point { x: 0.0, y: 0.0 },
            size: Size {
                width: 0.0,
                height: 0.0,
            },
        });
        Self {
            displays,
            primary_display_id,
            virtual_desktop_bounds,
            topology_generation,
        }
    }

    pub fn display(&self, id: &DisplayId) -> Result<&DisplayInfo, CoordinateTransformError> {
        self.displays
            .iter()
            .find(|display| &display.id == id)
            .ok_or_else(|| CoordinateTransformError::UnknownDisplay(id.clone()))
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenshotTarget {
    Display(DisplayId),
    VirtualDesktop,
}

/// The first raw capture representation.  This is deliberately descriptive
/// rather than a bag of backend bytes: producers must account for channel
/// order and row stride before handing a frame to a consumer.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FramePixelFormat {
    Bgra8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameEncoding {
    Png,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameState {
    Current,
    Released,
    Evicted,
    StaleTopology,
    Unknown,
}

/// Backend-neutral metadata for a captured, unencoded frame.  Raw pixels are
/// intentionally not part of Core or the RPC contract.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CaptureFrameMetadata {
    pub frame_id: FrameId,
    pub display_id: DisplayId,
    pub topology_generation: u64,
    pub coordinate_space: CoordinateSpace,
    pub desktop_origin: Point,
    pub width: u32,
    pub height: u32,
    pub pixel_format: FramePixelFormat,
    pub stride: u32,
    pub dpi: DpiScale,
    /// Display DPI scale, retained for diagnostics and UI sizing.
    pub scale: DpiScale,
    /// Explicit conversion from screenshot pixels to DesktopPhysical units.
    /// This is not always the display DPI scale: Windows captures physical
    /// pixels in a per-monitor-aware process, while macOS Quartz events use
    /// logical points.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_to_desktop_scale: Option<DpiScale>,
    pub captured_at: Option<SystemTime>,
    pub content_revision: u64,
    #[serde(default)]
    pub stale_topology: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameMetadataResult {
    pub metadata: Option<CaptureFrameMetadata>,
    pub state: FrameState,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub title: String,
    pub process_id: Option<u32>,
    /// Top-level window frame in DesktopPhysical coordinates.
    pub bounds: Coordinate,
    pub screen_id: Option<DisplayId>,
    pub active: bool,
    /// Security metadata is deliberately descriptive and backend-neutral.
    /// It never contains HWNDs, token handles, SIDs, or Windows token structs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<WindowSecurityMetadata>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntegrityLevel {
    Untrusted,
    Low,
    Medium,
    MediumPlus,
    High,
    System,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProcessSecurityContext {
    pub integrity_level: IntegrityLevel,
    pub elevated: bool,
    pub ui_access: bool,
    pub app_container: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesktopKind {
    InteractiveUserDesktop,
    ProtectedOrSecureDesktop,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DesktopSecurityContext {
    pub desktop_kind: DesktopKind,
    pub interactive: bool,
    pub protected: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerAccessBoundary {
    Allowed,
    IntegrityMismatch,
    ElevationRequired,
    ProtectedDesktop,
    TargetUnavailable,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum CapabilityAccess {
    Allowed,
    Denied { reason: String },
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetAccessCapabilities {
    pub pixel_observation: CapabilityAccess,
    pub semantic_observation: CapabilityAccess,
    pub window_focus: CapabilityAccess,
    pub pointer_input: CapabilityAccess,
    pub keyboard_input: CapabilityAccess,
    pub semantic_action: CapabilityAccess,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowSecurityMetadata {
    pub process: Option<ProcessSecurityContext>,
    pub boundary: ComputerAccessBoundary,
    pub capabilities: TargetAccessCapabilities,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SecurityDecision {
    pub operation: String,
    pub boundary: ComputerAccessBoundary,
    pub source: Option<ProcessSecurityContext>,
    pub target: Option<ProcessSecurityContext>,
    pub capabilities: TargetAccessCapabilities,
    pub reason: String,
}

/// A compatibility state is deliberately triaged independently from a bool:
/// an unknown provider or an unprobed capability must never be treated as
/// unsupported.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityStatus {
    Supported,
    Unsupported,
    Restricted,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilitySource {
    Static,
    Observed,
    Probed,
    SecurityPolicy,
    ProviderReported,
    HistoricalE2E,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProvenance {
    pub source: CapabilitySource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityAssessment {
    pub status: CapabilityStatus,
    pub provenance: CapabilityProvenance,
}

impl CapabilityAssessment {
    pub fn supported(source: CapabilitySource, detail: impl Into<String>) -> Self {
        Self {
            status: CapabilityStatus::Supported,
            provenance: CapabilityProvenance {
                source,
                detail: non_empty_detail(detail.into()),
            },
        }
    }

    pub fn unsupported(source: CapabilitySource, detail: impl Into<String>) -> Self {
        Self {
            status: CapabilityStatus::Unsupported,
            provenance: CapabilityProvenance {
                source,
                detail: non_empty_detail(detail.into()),
            },
        }
    }

    pub fn restricted(source: CapabilitySource, detail: impl Into<String>) -> Self {
        Self {
            status: CapabilityStatus::Restricted,
            provenance: CapabilityProvenance {
                source,
                detail: non_empty_detail(detail.into()),
            },
        }
    }

    pub fn unknown(source: CapabilitySource, detail: impl Into<String>) -> Self {
        Self {
            status: CapabilityStatus::Unknown,
            provenance: CapabilityProvenance {
                source,
                detail: non_empty_detail(detail.into()),
            },
        }
    }
}

fn non_empty_detail(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessArchitecture {
    X86,
    X64,
    Arm,
    Arm64,
    Unknown,
}

/// Framework classification is only a routing/diagnostic hint. It never
/// grants a capability without observation or provider evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameworkHint {
    Win32,
    WinUi,
    Wpf,
    WebView2,
    ChromiumElectron,
    UwpXaml,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationIdentity {
    pub process_id: Option<u32>,
    pub executable_name: Option<String>,
    pub executable_path: Option<String>,
    pub executable_hash: Option<String>,
    pub process_architecture: ProcessArchitecture,
    pub top_level_window_class: Option<String>,
    pub framework_hints: Vec<FrameworkHint>,
    pub version: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityObservationProfile {
    pub window: CapabilityAssessment,
    pub pixel: CapabilityAssessment,
    pub semantic: CapabilityAssessment,
    pub frame: CapabilityAssessment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityInputProfile {
    pub pointer: CapabilityAssessment,
    pub keyboard: CapabilityAssessment,
    pub text: CapabilityAssessment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilitySemanticActionProfile {
    pub focus: CapabilityAssessment,
    pub invoke: CapabilityAssessment,
    pub set_value: CapabilityAssessment,
    pub toggle: CapabilityAssessment,
    pub select: CapabilityAssessment,
    pub expand_collapse: CapabilityAssessment,
    pub range_value: CapabilityAssessment,
    pub scroll_into_view: CapabilityAssessment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityExecutionProfile {
    pub semantic_preferred: CapabilityAssessment,
    pub pixel_fallback_available: CapabilityAssessment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityEnvironmentProfile {
    pub access_boundary: ComputerAccessBoundary,
    pub display_compatibility: CapabilityAssessment,
    pub topology_compatibility: CapabilityAssessment,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CapabilityRestrictions {
    pub elevation_required: bool,
    pub foreground_required: bool,
    pub reasons: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct CapabilityProbeTimings {
    pub window_probe_ms: u128,
    pub uia_probe_ms: u128,
    pub security_probe_ms: u128,
    pub total_ms: u128,
}

/// Read-only, session-scoped compatibility information for one application
/// window. This is intentionally backend-neutral: no HWND, COM interface, or
/// UIA provider type crosses this boundary.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ApplicationCapabilityProfile {
    pub window_id: WindowId,
    pub application: ApplicationIdentity,
    pub framework_hint: FrameworkHint,
    pub observation: CapabilityObservationProfile,
    pub input: CapabilityInputProfile,
    pub semantic_actions: CapabilitySemanticActionProfile,
    pub execution: CapabilityExecutionProfile,
    pub environment: CapabilityEnvironmentProfile,
    pub restrictions: CapabilityRestrictions,
    pub security: Option<WindowSecurityMetadata>,
    pub semantic_generation: Option<u64>,
    pub cache_hit: bool,
    pub timings: CapabilityProbeTimings,
    pub known_gaps: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScreenshotMetadata {
    pub frame_id: FrameId,
    pub display_id: Option<DisplayId>,
    pub width: u32,
    pub height: u32,
    pub mime_type: String,
    pub coordinate_space: CoordinateSpace,
    pub desktop_origin: Point,
    pub dpi: DpiScale,
    /// Display DPI scale, retained for diagnostics and UI sizing.
    pub scale: DpiScale,
    /// Explicit conversion from screenshot pixels to DesktopPhysical units.
    /// Older payloads fall back to `scale` for backward compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pixel_to_desktop_scale: Option<DpiScale>,
    pub captured_at: Option<SystemTime>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Screenshot {
    pub metadata: ScreenshotMetadata,
    /// Encoded image bytes as returned by the backend (R0 uses PNG).
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FrameEncodingResult {
    pub screenshot: Screenshot,
    pub encoding: FrameEncoding,
    pub cache_hit: bool,
    pub encode_micros: u128,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerObservation {
    pub session_id: ComputerSessionId,
    pub screens: Vec<Screen>,
    pub windows: Vec<Window>,
    pub active_window: Option<WindowId>,
    pub screenshot: Option<Screenshot>,
    /// Metadata for the raw capture associated with this observation.  An
    /// observation does not imply PNG encoding or image-byte transport.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frame: Option<CaptureFrameMetadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_topology: Option<DisplayTopology>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoordinateTransformError {
    UnknownDisplay(DisplayId),
    MissingDisplayId(CoordinateSpace),
    MissingFrameId,
    InvalidFrame(String),
    InvalidGeometry(String),
}

impl fmt::Display for CoordinateTransformError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownDisplay(id) => write!(f, "unknown display id: {id}"),
            Self::MissingDisplayId(space) => write!(f, "{space:?} requires display_id"),
            Self::MissingFrameId => f.write_str("ScreenshotPixel requires frame_id"),
            Self::InvalidFrame(detail) => write!(f, "invalid screenshot frame: {detail}"),
            Self::InvalidGeometry(detail) => write!(f, "invalid display geometry: {detail}"),
        }
    }
}

impl std::error::Error for CoordinateTransformError {}

/// Pure, backend-neutral coordinate transforms.  It deliberately performs no
/// clamping: callers must handle an out-of-bounds or stale target explicitly.
pub struct CoordinateTransform<'a> {
    topology: &'a DisplayTopology,
}

impl<'a> CoordinateTransform<'a> {
    pub fn new(topology: &'a DisplayTopology) -> Self {
        Self { topology }
    }

    pub fn display_logical_to_physical(
        &self,
        display_id: &DisplayId,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        validate_scale(display.scale)?;
        Ok(Point {
            x: (point.x * display.scale.x).round(),
            y: (point.y * display.scale.y).round(),
        })
    }

    pub fn display_physical_to_logical(
        &self,
        display_id: &DisplayId,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        validate_scale(display.scale)?;
        Ok(Point {
            x: point.x / display.scale.x,
            y: point.y / display.scale.y,
        })
    }

    pub fn display_physical_to_desktop(
        &self,
        display_id: &DisplayId,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        Ok(Point {
            x: display.physical_bounds.origin.x + point.x,
            y: display.physical_bounds.origin.y + point.y,
        })
    }

    pub fn desktop_to_display_physical(
        &self,
        display_id: &DisplayId,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        Ok(Point {
            x: point.x - display.physical_bounds.origin.x,
            y: point.y - display.physical_bounds.origin.y,
        })
    }

    pub fn screenshot_to_desktop(
        &self,
        metadata: &ScreenshotMetadata,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        if metadata.coordinate_space != CoordinateSpace::ScreenshotPixel {
            return Err(CoordinateTransformError::InvalidFrame(
                "metadata coordinate_space is not screenshot_pixel".into(),
            ));
        }
        let scale = screenshot_pixel_scale(metadata)?;
        if point.x < 0.0
            || point.y < 0.0
            || point.x >= metadata.width as f64
            || point.y >= metadata.height as f64
        {
            return Err(CoordinateTransformError::InvalidGeometry(
                "screenshot point lies outside the frame".into(),
            ));
        }
        Ok(Point {
            x: metadata.desktop_origin.x + point.x / scale.x,
            y: metadata.desktop_origin.y + point.y / scale.y,
        })
    }

    pub fn desktop_to_screenshot(
        &self,
        metadata: &ScreenshotMetadata,
        point: Point,
    ) -> Result<Point, CoordinateTransformError> {
        let scale = screenshot_pixel_scale(metadata)?;
        let local = Point {
            x: (point.x - metadata.desktop_origin.x) * scale.x,
            y: (point.y - metadata.desktop_origin.y) * scale.y,
        };
        self.screenshot_to_desktop(metadata, local).map(|_| Point {
            x: local.x.round(),
            y: local.y.round(),
        })
    }

    pub fn display_physical_rect_to_desktop(
        &self,
        display_id: &DisplayId,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let origin = self.display_physical_to_desktop(display_id, rect.origin)?;
        Ok(Rect {
            origin,
            size: rect.size,
        })
    }

    pub fn display_logical_rect_to_physical(
        &self,
        display_id: &DisplayId,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        validate_scale(display.scale)?;
        let left = (rect.origin.x * display.scale.x).floor();
        let top = (rect.origin.y * display.scale.y).floor();
        let right = ((rect.origin.x + rect.size.width) * display.scale.x).ceil();
        let bottom = ((rect.origin.y + rect.size.height) * display.scale.y).ceil();
        Ok(Rect {
            origin: Point { x: left, y: top },
            size: Size {
                width: (right - left).max(0.0),
                height: (bottom - top).max(0.0),
            },
        })
    }

    pub fn display_physical_rect_to_logical(
        &self,
        display_id: &DisplayId,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let display = self.topology.display(display_id)?;
        validate_scale(display.scale)?;
        let left = rect.origin.x / display.scale.x;
        let top = rect.origin.y / display.scale.y;
        let right = (rect.origin.x + rect.size.width) / display.scale.x;
        let bottom = (rect.origin.y + rect.size.height) / display.scale.y;
        Ok(Rect {
            origin: Point { x: left, y: top },
            size: Size {
                width: (right - left).max(0.0),
                height: (bottom - top).max(0.0),
            },
        })
    }

    pub fn display_logical_rect_to_desktop(
        &self,
        display_id: &DisplayId,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let physical = self.display_logical_rect_to_physical(display_id, rect)?;
        self.display_physical_rect_to_desktop(display_id, physical)
    }

    pub fn screenshot_rect_to_desktop(
        &self,
        metadata: &ScreenshotMetadata,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let scale = screenshot_pixel_scale(metadata)?;
        validate_screenshot_rect(metadata, rect)?;
        let origin = self.screenshot_to_desktop(metadata, rect.origin)?;
        Ok(Rect {
            origin,
            size: Size {
                width: rect.size.width / scale.x,
                height: rect.size.height / scale.y,
            },
        })
    }

    pub fn desktop_rect_to_screenshot(
        &self,
        metadata: &ScreenshotMetadata,
        rect: Rect,
    ) -> Result<Rect, CoordinateTransformError> {
        let scale = screenshot_pixel_scale(metadata)?;
        let local = Rect {
            origin: Point {
                x: (rect.origin.x - metadata.desktop_origin.x) * scale.x,
                y: (rect.origin.y - metadata.desktop_origin.y) * scale.y,
            },
            size: Size {
                width: rect.size.width * scale.x,
                height: rect.size.height * scale.y,
            },
        };
        validate_screenshot_rect(metadata, local)?;
        let left = local.origin.x.floor();
        let top = local.origin.y.floor();
        let right = (local.origin.x + local.size.width).ceil();
        let bottom = (local.origin.y + local.size.height).ceil();
        Ok(Rect {
            origin: Point { x: left, y: top },
            size: Size {
                width: (right - left).max(0.0),
                height: (bottom - top).max(0.0),
            },
        })
    }
}

fn validate_scale(scale: DpiScale) -> Result<(), CoordinateTransformError> {
    if scale.x.is_finite() && scale.y.is_finite() && scale.x > 0.0 && scale.y > 0.0 {
        Ok(())
    } else {
        Err(CoordinateTransformError::InvalidGeometry(
            "display scale must be finite and positive".into(),
        ))
    }
}

fn validate_screenshot_rect(
    metadata: &ScreenshotMetadata,
    rect: Rect,
) -> Result<(), CoordinateTransformError> {
    if metadata.coordinate_space != CoordinateSpace::ScreenshotPixel {
        return Err(CoordinateTransformError::InvalidFrame(
            "metadata coordinate_space is not screenshot_pixel".into(),
        ));
    }
    let right = rect.origin.x + rect.size.width;
    let bottom = rect.origin.y + rect.size.height;
    if rect.origin.x < 0.0
        || rect.origin.y < 0.0
        || rect.size.width < 0.0
        || rect.size.height < 0.0
        || right > metadata.width as f64
        || bottom > metadata.height as f64
    {
        return Err(CoordinateTransformError::InvalidGeometry(
            "screenshot rect lies outside the frame".into(),
        ));
    }
    Ok(())
}

fn screenshot_pixel_scale(
    metadata: &ScreenshotMetadata,
) -> Result<DpiScale, CoordinateTransformError> {
    let scale = metadata.pixel_to_desktop_scale.unwrap_or(metadata.scale);
    validate_scale(scale)?;
    Ok(scale)
}

fn union_rects(rects: Vec<Rect>) -> Option<Rect> {
    let first = *rects.first()?;
    let mut left = first.origin.x;
    let mut top = first.origin.y;
    let mut right = first.origin.x + first.size.width;
    let mut bottom = first.origin.y + first.size.height;
    for rect in rects.into_iter().skip(1) {
        left = left.min(rect.origin.x);
        top = top.min(rect.origin.y);
        right = right.max(rect.origin.x + rect.size.width);
        bottom = bottom.max(rect.origin.y + rect.size.height);
    }
    Some(Rect {
        origin: Point { x: left, y: top },
        size: Size {
            width: (right - left).max(0.0),
            height: (bottom - top).max(0.0),
        },
    })
}

impl From<CoordinateTransformError> for ComputerError {
    fn from(error: CoordinateTransformError) -> Self {
        match error {
            CoordinateTransformError::UnknownDisplay(id) => Self::UnknownDisplay(id.to_string()),
            CoordinateTransformError::MissingDisplayId(space) => {
                Self::InvalidCoordinate(format!("{space:?} coordinate is missing display_id"))
            }
            CoordinateTransformError::MissingFrameId => {
                Self::InvalidCoordinate("screenshot_pixel coordinate is missing frame_id".into())
            }
            CoordinateTransformError::InvalidFrame(detail)
            | CoordinateTransformError::InvalidGeometry(detail) => Self::InvalidCoordinate(detail),
        }
    }
}

/// The bounded, backend-neutral capabilities exposed by one semantic element.
///
/// Semantic actions use these capabilities as admission gates. Pattern
/// objects and platform interfaces remain outside this backend-neutral crate.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticCapabilities {
    pub invokable: bool,
    pub editable: bool,
    pub selectable: bool,
    pub scrollable: bool,
    pub expandable: bool,
    pub toggleable: bool,
    pub range_adjustable: bool,
    pub scroll_into_view: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerElement {
    pub id: ElementId,
    pub parent_id: Option<ElementId>,
    pub child_ids: Vec<ElementId>,
    pub role: String,
    pub control_type: String,
    pub name: Option<String>,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    /// A bounded, non-sensitive value summary. Password controls never carry
    /// a value or text summary.
    pub value_summary: Option<String>,
    /// A bounded, non-sensitive text summary. Password controls never carry
    /// a value or text summary.
    pub text_summary: Option<String>,
    pub bounds: Option<Coordinate>,
    pub enabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub offscreen: bool,
    pub toggle_state: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub range_value: Option<f64>,
    pub range_minimum: Option<f64>,
    pub range_maximum: Option<f64>,
    pub capabilities: SemanticCapabilities,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticObservationLimits {
    pub max_depth: u32,
    pub max_elements: u32,
}

impl Default for SemanticObservationLimits {
    fn default() -> Self {
        Self {
            max_depth: 8,
            max_elements: 512,
        }
    }
}

impl SemanticObservationLimits {
    pub const MAX_DEPTH: u32 = 64;
    pub const MAX_ELEMENTS: u32 = 4096;

    pub fn bounded(self) -> Self {
        Self {
            max_depth: self.max_depth.min(Self::MAX_DEPTH),
            max_elements: self.max_elements.clamp(1, Self::MAX_ELEMENTS),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticObservationMetadata {
    pub captured_at: Option<SystemTime>,
    pub generation: u64,
    pub max_depth: u32,
    pub max_elements: u32,
    pub truncated: bool,
    pub uia_init_micros: u128,
    pub tree_walk_micros: u128,
    pub property_read_micros: u128,
    pub serialization_micros: Option<u128>,
    pub total_micros: u128,
    pub element_count: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticObservation {
    pub session_id: ComputerSessionId,
    pub window_id: WindowId,
    pub root_element_id: ElementId,
    pub elements: Vec<ComputerElement>,
    pub metadata: SemanticObservationMetadata,
}

/// Read-only semantic actions operate on an ElementId from the caller's
/// current observation.  The backend may execute these through a native
/// semantic provider, but the contract never exposes that provider's types.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum SemanticAction {
    Focus {
        element_id: ElementId,
    },
    Invoke {
        element_id: ElementId,
    },
    SetValue {
        element_id: ElementId,
        value: String,
    },
    Toggle {
        element_id: ElementId,
    },
    Select {
        element_id: ElementId,
    },
    Expand {
        element_id: ElementId,
    },
    Collapse {
        element_id: ElementId,
    },
    SetRangeValue {
        element_id: ElementId,
        value: f64,
    },
    ScrollIntoView {
        element_id: ElementId,
    },
}

impl SemanticAction {
    pub fn element_id(&self) -> &ElementId {
        match self {
            Self::Focus { element_id }
            | Self::Invoke { element_id }
            | Self::SetValue { element_id, .. }
            | Self::Toggle { element_id }
            | Self::Select { element_id }
            | Self::Expand { element_id }
            | Self::Collapse { element_id }
            | Self::SetRangeValue { element_id, .. }
            | Self::ScrollIntoView { element_id } => element_id,
        }
    }
}

impl ApplicationCapabilityProfile {
    pub fn semantic_action(&self, action: &SemanticAction) -> &CapabilityAssessment {
        match action {
            SemanticAction::Focus { .. } => &self.semantic_actions.focus,
            SemanticAction::Invoke { .. } => &self.semantic_actions.invoke,
            SemanticAction::SetValue { .. } => &self.semantic_actions.set_value,
            SemanticAction::Toggle { .. } => &self.semantic_actions.toggle,
            SemanticAction::Select { .. } => &self.semantic_actions.select,
            SemanticAction::Expand { .. } | SemanticAction::Collapse { .. } => {
                &self.semantic_actions.expand_collapse
            }
            SemanticAction::SetRangeValue { .. } => &self.semantic_actions.range_value,
            SemanticAction::ScrollIntoView { .. } => &self.semantic_actions.scroll_into_view,
        }
    }

    pub fn apply_security_restriction(&mut self, reason: impl Into<String>) {
        let reason = reason.into();
        let restricted = |detail: &str| {
            CapabilityAssessment::restricted(CapabilitySource::SecurityPolicy, detail)
        };
        self.input.pointer = restricted(&reason);
        self.input.keyboard = restricted(&reason);
        self.input.text = restricted(&reason);
        self.semantic_actions.focus = restricted(&reason);
        self.semantic_actions.invoke = restricted(&reason);
        self.semantic_actions.set_value = restricted(&reason);
        self.semantic_actions.toggle = restricted(&reason);
        self.semantic_actions.select = restricted(&reason);
        self.semantic_actions.expand_collapse = restricted(&reason);
        self.semantic_actions.range_value = restricted(&reason);
        self.semantic_actions.scroll_into_view = restricted(&reason);
        self.execution.semantic_preferred = restricted(&reason);
        self.execution.pixel_fallback_available = restricted(&reason);
        self.restrictions.reasons.push(reason);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticActionStatus {
    Performed,
    Unsupported,
    StaleElement,
    UnknownElement,
    ElementUnavailable,
    Disabled,
    ReadOnly,
    InvalidValue,
    /// The containing top-level window is not the current foreground window.
    /// Semantic Focus must fail before calling the provider's focus primitive.
    WindowNotForeground,
    FocusDenied,
    VerificationFailed,
    OutcomeUnknown,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SemanticActionVerification {
    pub verified: bool,
    pub detail: Option<String>,
    pub focused: Option<bool>,
    pub state_changed: Option<bool>,
    pub observed_value: Option<String>,
    pub toggled: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub range_value: Option<f64>,
    pub offscreen: Option<bool>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct SemanticActionTiming {
    pub element_resolve_micros: u128,
    pub pattern_acquire_micros: u128,
    pub action_micros: u128,
    pub verification_micros: u128,
    pub refresh_micros: u128,
    pub total_micros: u128,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SemanticActionResult {
    pub action: SemanticAction,
    pub element_id: ElementId,
    pub status: SemanticActionStatus,
    pub verification: SemanticActionVerification,
    pub timing: SemanticActionTiming,
    pub observation_generation_before: Option<u64>,
    pub observation_generation_after: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityDecision>,
}

/// The policy selected by the caller for one explicit execution request.
/// This is a routing policy, not an autonomous decision or a risk model.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerExecutionStrategy {
    SemanticOnly,
    PixelOnly,
    #[default]
    PreferSemantic,
    PreferPixel,
}

/// Selects whether a native backend should keep the user's desktop untouched
/// when it has an application-scoped or Accessibility-based route available.
/// `TakeoverOnly` is retained for compatibility and for the last-resort path
/// required by controls that cannot be operated in the background.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerExecutionMode {
    #[default]
    BackgroundPreferred,
    TakeoverOnly,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerFallbackPolicy {
    Allow,
    #[default]
    Deny,
    RequireExplicit,
}

/// An execution intent is either an explicit semantic action or an explicit
/// pixel action.  Fallback from the former to the latter is performed only by
/// the runtime policy and only after the caller's policy admits it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComputerExecutionIntent {
    Semantic(SemanticAction),
    Pixel {
        action: ComputerAction,
        target_window_id: Option<WindowId>,
        /// Optional application-level target for keyboard/text input.  This
        /// is intentionally separate from `target_window_id`: a browser can
        /// create a new top-level window while remaining the same process,
        /// whereas screenshot coordinates must stay bound to one window.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_application: Option<Box<ApplicationIdentity>>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerExecutionRequest {
    pub intent: ComputerExecutionIntent,
    pub strategy: ComputerExecutionStrategy,
    pub fallback_policy: ComputerFallbackPolicy,
    #[serde(default)]
    pub execution_mode: ComputerExecutionMode,
}

impl ComputerExecutionRequest {
    pub fn semantic(action: SemanticAction) -> Self {
        Self {
            intent: ComputerExecutionIntent::Semantic(action),
            strategy: ComputerExecutionStrategy::PreferSemantic,
            fallback_policy: ComputerFallbackPolicy::Deny,
            execution_mode: ComputerExecutionMode::BackgroundPreferred,
        }
    }

    pub fn pixel(action: ComputerAction, target_window_id: Option<WindowId>) -> Self {
        Self {
            intent: ComputerExecutionIntent::Pixel {
                action,
                target_window_id,
                target_application: None,
            },
            strategy: ComputerExecutionStrategy::PixelOnly,
            fallback_policy: ComputerFallbackPolicy::Deny,
            execution_mode: ComputerExecutionMode::BackgroundPreferred,
        }
    }

    /// Construct an application-scoped keyboard/text request. The macOS
    /// runtime delivers these events to the application's PID when possible;
    /// other platforms retain their native foreground contract. Spatial
    /// actions must remain window/pixel scoped and are rejected by the backend
    /// if passed here.
    pub fn application(action: ComputerAction, target: ApplicationIdentity) -> Self {
        Self {
            intent: ComputerExecutionIntent::Pixel {
                action,
                target_window_id: None,
                target_application: Some(Box::new(target)),
            },
            strategy: ComputerExecutionStrategy::PixelOnly,
            fallback_policy: ComputerFallbackPolicy::Deny,
            execution_mode: ComputerExecutionMode::BackgroundPreferred,
        }
    }

    pub fn background_preferred(mut self) -> Self {
        self.execution_mode = ComputerExecutionMode::BackgroundPreferred;
        self
    }

    pub fn with_execution_mode(mut self, mode: ComputerExecutionMode) -> Self {
        self.execution_mode = mode;
        self
    }

    pub fn takeover_only(mut self) -> Self {
        self.execution_mode = ComputerExecutionMode::TakeoverOnly;
        self
    }

    pub fn allows_background(&self) -> bool {
        self.execution_mode == ComputerExecutionMode::BackgroundPreferred
    }

    pub fn target_application(&self) -> Option<&ApplicationIdentity> {
        match &self.intent {
            ComputerExecutionIntent::Pixel {
                target_application, ..
            } => target_application.as_deref(),
            ComputerExecutionIntent::Semantic(_) => None,
        }
    }

    pub fn semantic_element_id(&self) -> Option<&ElementId> {
        match &self.intent {
            ComputerExecutionIntent::Semantic(action) => Some(action.element_id()),
            ComputerExecutionIntent::Pixel { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerExecutionMethod {
    SemanticFocus,
    SemanticInvoke,
    SemanticSetValue,
    SemanticToggle,
    SemanticSelect,
    SemanticExpand,
    SemanticCollapse,
    SemanticSetRangeValue,
    SemanticScrollIntoView,
    PixelClick,
    PixelAction,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerExecutionOutcome {
    Performed,
    Unsupported,
    FallbackDenied,
    StaleElement,
    UnknownElement,
    ElementUnavailable,
    Disabled,
    ReadOnly,
    InvalidValue,
    WindowNotForeground,
    FocusDenied,
    VerificationFailed,
    OutcomeUnknown,
    InvalidRequest,
    InvalidTarget,
    BackendError,
    IntegrityMismatch,
    ElevationRequired,
    UipiDenied,
    ProtectedDesktop,
    SecurityContextUnavailable,
    TargetUnavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputerExecutionVerificationKind {
    SemanticStateChanged,
    Focused,
    ValueEquals,
    ElementDisappeared,
    ElementAppeared,
    WindowChanged,
    CustomFixtureState,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ComputerExecutionVerification {
    pub kind: Option<ComputerExecutionVerificationKind>,
    pub verified: bool,
    pub detail: Option<String>,
    pub state_changed: Option<bool>,
    pub focused: Option<bool>,
    pub observed_value: Option<String>,
    pub toggled: Option<bool>,
    pub selected: Option<bool>,
    pub expanded: Option<bool>,
    pub range_value: Option<f64>,
    pub offscreen: Option<bool>,
}

/// A pixel target derived from the current semantic observation.  The target
/// is valid only for the reported element generation and containing window.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PixelTarget {
    pub element_id: ElementId,
    pub window_id: WindowId,
    pub bounds: Coordinate,
    pub center: Coordinate,
    pub generation: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerExecutionAttempt {
    pub method: ComputerExecutionMethod,
    pub outcome: ComputerExecutionOutcome,
    pub semantic_element_id: Option<ElementId>,
    pub pixel_target: Option<PixelTarget>,
    pub verification: ComputerExecutionVerification,
    pub generation_before: Option<u64>,
    pub generation_after: Option<u64>,
    pub timing_ms: u128,
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ComputerExecutionTiming {
    pub policy_ms: u128,
    pub semantic_attempt_ms: u128,
    pub fallback_decision_ms: u128,
    pub pixel_attempt_ms: u128,
    pub verification_ms: u128,
    pub total_ms: u128,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerExecutionResult {
    pub requested_intent: ComputerExecutionIntent,
    pub selected_strategy: ComputerExecutionStrategy,
    pub attempts: Vec<ComputerExecutionAttempt>,
    pub final_outcome: ComputerExecutionOutcome,
    pub semantic_element_id: Option<ElementId>,
    pub pixel_target: Option<PixelTarget>,
    pub verification: ComputerExecutionVerification,
    pub fallback_used: bool,
    pub generation_before: Option<u64>,
    pub generation_after: Option<u64>,
    pub timing: ComputerExecutionTiming,
    pub explanation: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security: Option<SecurityDecision>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// A screenshot-driven pointer gesture with optional modifier keys. This is
/// intentionally separate from the legacy actions so a computer-use batch can
/// preserve signed scroll deltas and every drag waypoint without changing the
/// established wire shape of the legacy Drag and Scroll actions.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComputerPointerAction {
    Click {
        at: Coordinate,
        button: MouseButton,
        clicks: u8,
    },
    Move {
        to: Coordinate,
    },
    Drag {
        path: Vec<Coordinate>,
        button: MouseButton,
    },
    Scroll {
        at: Coordinate,
        /// API deltas: positive delta_y scrolls down and positive
        /// delta_x scrolls right, matching browser computer-use semantics.
        delta_x: i32,
        delta_y: i32,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum ComputerAction {
    Click {
        at: Coordinate,
    },
    DoubleClick {
        at: Coordinate,
    },
    RightClick {
        at: Coordinate,
    },
    MovePointer {
        to: Coordinate,
    },
    Drag {
        from: Coordinate,
        to: Coordinate,
        button: MouseButton,
    },
    Scroll {
        at: Coordinate,
        direction: ScrollDirection,
        amount: u32,
    },
    TypeText {
        text: String,
        target: Option<WindowId>,
        /// Optional current screenshot coordinate to click before typing.
        /// When present, the backend performs pointer placement, click, focus
        /// settling, and text injection as one action/lease.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        at: Option<Coordinate>,
    },
    KeyPress {
        key: String,
        target: Option<WindowId>,
    },
    Hotkey {
        keys: Vec<String>,
        target: Option<WindowId>,
    },
    MouseDown {
        button: MouseButton,
        at: Coordinate,
        target: Option<WindowId>,
    },
    MouseUp {
        button: MouseButton,
        at: Coordinate,
        target: Option<WindowId>,
    },
    MiddleClick {
        at: Coordinate,
        target: Option<WindowId>,
    },
    TripleClick {
        at: Coordinate,
        target: Option<WindowId>,
    },
    ModifierClick {
        modifier: String,
        button: MouseButton,
        at: Coordinate,
        target: Option<WindowId>,
    },
    KeyDown {
        key: String,
        target: Option<WindowId>,
    },
    KeyUp {
        key: String,
        target: Option<WindowId>,
    },
    HoldKey {
        key: String,
        duration_ms: u32,
        target: Option<WindowId>,
    },
    FocusWindow {
        window_id: WindowId,
    },
    ModifiedPointer {
        action: ComputerPointerAction,
        modifiers: Vec<String>,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionStatus {
    Performed,
    Refused,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ComputerActionResult {
    pub status: ActionStatus,
    pub observation: Option<ComputerObservation>,
    pub backend_detail: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComputerError {
    InvalidAction(String),
    InvalidWindow(String),
    UnknownDisplay(String),
    StaleDisplay(String),
    StaleElement(String),
    UnknownElement(String),
    InvalidCoordinate(String),
    SessionClosed,
    NotInitialized,
    AlreadyInitialized,
    ForegroundDenied(String),
    FocusNotAcquired(String),
    Unsupported { capability: String, detail: String },
    CapabilityGap { capability: String, detail: String },
    IntegrityMismatch(String),
    ElevationRequired(String),
    UipiDenied(String),
    ProtectedDesktop(String),
    SecurityContextUnavailable(String),
    AccessDenied(String),
    TargetUnavailable(String),
    Backend(String),
}

impl fmt::Display for ComputerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAction(detail) => write!(f, "invalid computer action: {detail}"),
            Self::InvalidWindow(detail) => write!(f, "invalid window: {detail}"),
            Self::UnknownDisplay(detail) => write!(f, "unknown display: {detail}"),
            Self::StaleDisplay(detail) => write!(f, "stale display: {detail}"),
            Self::StaleElement(detail) => write!(f, "stale element: {detail}"),
            Self::UnknownElement(detail) => write!(f, "unknown element: {detail}"),
            Self::InvalidCoordinate(detail) => write!(f, "invalid coordinate: {detail}"),
            Self::SessionClosed => f.write_str("computer session is closed"),
            Self::NotInitialized => f.write_str("computer backend is not initialized"),
            Self::AlreadyInitialized => f.write_str("computer backend is already initialized"),
            Self::ForegroundDenied(detail) => write!(f, "foreground denied: {detail}"),
            Self::FocusNotAcquired(detail) => write!(f, "focus not acquired: {detail}"),
            Self::Unsupported { capability, detail } => {
                write!(f, "unsupported {capability}: {detail}")
            }
            Self::CapabilityGap { capability, detail } => {
                write!(f, "capability gap {capability}: {detail}")
            }
            Self::IntegrityMismatch(detail) => write!(f, "integrity mismatch: {detail}"),
            Self::ElevationRequired(detail) => write!(f, "elevation required: {detail}"),
            Self::UipiDenied(detail) => write!(f, "UIPI denied: {detail}"),
            Self::ProtectedDesktop(detail) => write!(f, "protected desktop: {detail}"),
            Self::SecurityContextUnavailable(detail) => {
                write!(f, "security context unavailable: {detail}")
            }
            Self::AccessDenied(detail) => write!(f, "access denied: {detail}"),
            Self::TargetUnavailable(detail) => write!(f, "target unavailable: {detail}"),
            Self::Backend(detail) => write!(f, "computer backend error: {detail}"),
        }
    }
}

impl std::error::Error for ComputerError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coordinate_serialization_keeps_space_extent_and_dpi() {
        let coordinate = Coordinate {
            space: CoordinateSpace::Window,
            point: Point { x: 10.0, y: 20.0 },
            extent: Size {
                width: 800.0,
                height: 600.0,
            },
            dpi: DpiScale { x: 1.25, y: 1.25 },
            display_id: None,
            frame_id: None,
        };
        let json = serde_json::to_value(coordinate).unwrap();
        assert_eq!(json["space"], "window");
        assert_eq!(json["extent"]["width"], 800.0);
        assert_eq!(json["dpi"]["x"], 1.25);
    }

    #[test]
    fn type_text_coordinate_is_optional_and_backward_compatible() {
        let legacy = serde_json::json!({
            "TypeText": {
                "text": "hello",
                "target": "window-1"
            }
        });
        let action: ComputerAction = serde_json::from_value(legacy).unwrap();
        assert!(matches!(action, ComputerAction::TypeText { at: None, .. }));

        let spatial = ComputerAction::TypeText {
            text: "hello".into(),
            target: Some(WindowId::new("window-1")),
            at: Some(Coordinate {
                space: CoordinateSpace::ScreenshotPixel,
                point: Point { x: 10.0, y: 20.0 },
                extent: Size {
                    width: 100.0,
                    height: 100.0,
                },
                dpi: DpiScale::ONE,
                display_id: Some(DisplayId::new("display-1")),
                frame_id: Some(FrameId::new("frame-1")),
            }),
        };
        assert!(serde_json::to_value(spatial).unwrap()["TypeText"]["at"].is_object());
    }

    #[test]
    fn execution_mode_defaults_to_background_and_accepts_legacy_requests() {
        let request = ComputerExecutionRequest::pixel(
            ComputerAction::KeyPress {
                key: "a".into(),
                target: None,
            },
            None,
        );
        assert_eq!(
            request.execution_mode,
            ComputerExecutionMode::BackgroundPreferred
        );
        let mut legacy = serde_json::to_value(&request).unwrap();
        legacy
            .as_object_mut()
            .expect("request is an object")
            .remove("execution_mode");
        let decoded: ComputerExecutionRequest = serde_json::from_value(legacy).unwrap();
        assert!(decoded.allows_background());
        assert_eq!(
            decoded.takeover_only().execution_mode,
            ComputerExecutionMode::TakeoverOnly
        );
    }

    fn display(id: &str, origin: Point, size: Size, scale: f64, primary: bool) -> DisplayInfo {
        DisplayInfo {
            id: DisplayId::new(id),
            name: Some(id.into()),
            physical_bounds: Rect { origin, size },
            work_area: Rect { origin, size },
            physical_size: size,
            logical_size: Size {
                width: size.width / scale,
                height: size.height / scale,
            },
            dpi: DpiScale {
                x: scale * 96.0,
                y: scale * 96.0,
            },
            scale: DpiScale { x: scale, y: scale },
            primary,
        }
    }

    #[test]
    fn synthetic_topology_supports_negative_and_mixed_dpi_displays() {
        let topology = DisplayTopology::from_displays(
            vec![
                display(
                    "primary",
                    Point { x: 0.0, y: 0.0 },
                    Size {
                        width: 2560.0,
                        height: 1600.0,
                    },
                    1.5,
                    true,
                ),
                display(
                    "left",
                    Point { x: -1920.0, y: 0.0 },
                    Size {
                        width: 1920.0,
                        height: 1080.0,
                    },
                    1.0,
                    false,
                ),
                display(
                    "above",
                    Point { x: 0.0, y: -1080.0 },
                    Size {
                        width: 1920.0,
                        height: 1080.0,
                    },
                    1.25,
                    false,
                ),
            ],
            7,
        );
        assert_eq!(
            topology.virtual_desktop_bounds.origin,
            Point {
                x: -1920.0,
                y: -1080.0
            }
        );
        assert_eq!(
            topology.virtual_desktop_bounds.size,
            Size {
                width: 4480.0,
                height: 2680.0
            }
        );

        let transform = CoordinateTransform::new(&topology);
        let left = transform
            .display_physical_to_desktop(&DisplayId::new("left"), Point { x: 100.0, y: 200.0 })
            .unwrap();
        assert_eq!(
            left,
            Point {
                x: -1820.0,
                y: 200.0
            }
        );
        let logical = transform
            .display_logical_to_physical(&DisplayId::new("primary"), Point { x: 100.0, y: 100.0 })
            .unwrap();
        assert_eq!(logical, Point { x: 150.0, y: 150.0 });
        let back = transform
            .display_physical_to_logical(&DisplayId::new("primary"), logical)
            .unwrap();
        assert!((back.x - 100.0).abs() < 1.0 && (back.y - 100.0).abs() < 1.0);

        let cross = transform
            .display_physical_rect_to_desktop(
                &DisplayId::new("primary"),
                Rect {
                    origin: Point { x: 2400.0, y: 10.0 },
                    size: Size {
                        width: 500.0,
                        height: 40.0,
                    },
                },
            )
            .unwrap();
        assert_eq!(cross.size.width, 500.0);
        assert!(cross.origin.x + cross.size.width > 2560.0);
    }

    #[test]
    fn screenshot_frame_mapping_preserves_origin_and_rejects_out_of_bounds() {
        let topology = DisplayTopology::from_displays(Vec::new(), 1);
        let transform = CoordinateTransform::new(&topology);
        let metadata = ScreenshotMetadata {
            frame_id: FrameId::new("frame-1"),
            display_id: Some(DisplayId::new("left")),
            width: 1920,
            height: 1080,
            mime_type: "image/png".into(),
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: Point { x: -1920.0, y: 0.0 },
            dpi: DpiScale::ONE,
            scale: DpiScale::ONE,
            pixel_to_desktop_scale: Some(DpiScale::ONE),
            captured_at: None,
        };
        assert_eq!(
            transform
                .screenshot_to_desktop(&metadata, Point { x: 100.0, y: 200.0 })
                .unwrap(),
            Point {
                x: -1820.0,
                y: 200.0
            }
        );
        assert!(transform
            .screenshot_to_desktop(&metadata, Point { x: 1920.0, y: 0.0 })
            .is_err());
    }

    #[test]
    fn scaled_screenshot_frame_maps_pixels_to_desktop_units() {
        let topology = DisplayTopology::from_displays(Vec::new(), 1);
        let transform = CoordinateTransform::new(&topology);
        let metadata = ScreenshotMetadata {
            frame_id: FrameId::new("scaled-frame"),
            display_id: Some(DisplayId::new("retina")),
            width: 1920,
            height: 1080,
            mime_type: "image/png".into(),
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: Point { x: -960.0, y: 40.0 },
            dpi: DpiScale { x: 192.0, y: 144.0 },
            scale: DpiScale { x: 2.0, y: 1.5 },
            pixel_to_desktop_scale: Some(DpiScale { x: 2.0, y: 1.5 }),
            captured_at: None,
        };

        assert_eq!(
            transform
                .screenshot_to_desktop(&metadata, Point { x: 200.0, y: 150.0 })
                .unwrap(),
            Point {
                x: -860.0,
                y: 140.0
            }
        );
        assert_eq!(
            transform
                .desktop_to_screenshot(
                    &metadata,
                    Point {
                        x: -860.0,
                        y: 140.0
                    }
                )
                .unwrap(),
            Point { x: 200.0, y: 150.0 }
        );

        let screenshot_rect = transform
            .screenshot_rect_to_desktop(
                &metadata,
                Rect {
                    origin: Point { x: 200.0, y: 150.0 },
                    size: Size {
                        width: 400.0,
                        height: 300.0,
                    },
                },
            )
            .unwrap();
        assert_eq!(
            screenshot_rect.origin,
            Point {
                x: -860.0,
                y: 140.0
            }
        );
        assert_eq!(
            screenshot_rect.size,
            Size {
                width: 200.0,
                height: 200.0,
            }
        );
        assert_eq!(
            transform
                .desktop_rect_to_screenshot(&metadata, screenshot_rect)
                .unwrap(),
            Rect {
                origin: Point { x: 200.0, y: 150.0 },
                size: Size {
                    width: 400.0,
                    height: 300.0,
                },
            }
        );
    }

    #[test]
    fn screenshot_mapping_rejects_invalid_pixel_scale() {
        let topology = DisplayTopology::from_displays(Vec::new(), 1);
        let transform = CoordinateTransform::new(&topology);
        let metadata = ScreenshotMetadata {
            frame_id: FrameId::new("invalid-scale"),
            display_id: None,
            width: 10,
            height: 10,
            mime_type: "image/png".into(),
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: Point { x: 0.0, y: 0.0 },
            dpi: DpiScale::ONE,
            scale: DpiScale::ONE,
            pixel_to_desktop_scale: Some(DpiScale { x: 0.0, y: 1.0 }),
            captured_at: None,
        };
        assert!(transform
            .screenshot_to_desktop(&metadata, Point { x: 1.0, y: 1.0 })
            .is_err());
    }

    #[test]
    fn synthetic_r6_matrix_covers_right_left_above_mixed_dpi_and_stale_ids() {
        let displays = vec![
            display(
                "a",
                Point { x: 0.0, y: 0.0 },
                Size {
                    width: 2560.0,
                    height: 1600.0,
                },
                1.5,
                true,
            ),
            display(
                "b-right",
                Point { x: 2560.0, y: 0.0 },
                Size {
                    width: 1920.0,
                    height: 1080.0,
                },
                1.0,
                false,
            ),
            display(
                "c-left",
                Point { x: -1920.0, y: 0.0 },
                Size {
                    width: 1920.0,
                    height: 1080.0,
                },
                1.25,
                false,
            ),
            display(
                "d-above",
                Point { x: 0.0, y: -1080.0 },
                Size {
                    width: 1920.0,
                    height: 1080.0,
                },
                2.0,
                false,
            ),
        ];
        let topology = DisplayTopology::from_displays(displays.clone(), 11);
        let transform = CoordinateTransform::new(&topology);
        assert_eq!(
            transform
                .display_physical_to_desktop(&DisplayId::new("b-right"), Point { x: 10.0, y: 20.0 })
                .unwrap(),
            Point { x: 2570.0, y: 20.0 }
        );
        assert_eq!(
            transform
                .display_physical_to_desktop(
                    &DisplayId::new("c-left"),
                    Point { x: 100.0, y: 200.0 }
                )
                .unwrap(),
            Point {
                x: -1820.0,
                y: 200.0
            }
        );
        assert_eq!(
            transform
                .display_physical_to_desktop(
                    &DisplayId::new("d-above"),
                    Point { x: 100.0, y: 200.0 }
                )
                .unwrap(),
            Point {
                x: 100.0,
                y: -880.0
            }
        );
        let cross = transform
            .display_physical_rect_to_desktop(
                &DisplayId::new("a"),
                Rect {
                    origin: Point { x: 2400.0, y: 20.0 },
                    size: Size {
                        width: 500.0,
                        height: 80.0,
                    },
                },
            )
            .unwrap();
        assert_eq!(cross.size.width, 500.0);
        assert!(cross.origin.x < 2560.0 && cross.origin.x + cross.size.width > 2560.0);
        assert!(matches!(
            DisplayTopology::from_displays(vec![displays[0].clone()], 12)
                .display(&DisplayId::new("c-left")),
            Err(CoordinateTransformError::UnknownDisplay(_))
        ));
    }

    #[test]
    fn capability_states_keep_unknown_and_security_restricted_distinct() {
        let unknown =
            CapabilityAssessment::unknown(CapabilitySource::Probed, "provider not probed");
        let encoded = serde_json::to_value(&unknown).unwrap();
        assert_eq!(encoded["status"], "unknown");
        assert_eq!(encoded["provenance"]["source"], "probed");

        let supported = CapabilityAssessment::supported(CapabilitySource::Observed, "fixture");
        let mut profile = ApplicationCapabilityProfile {
            window_id: WindowId::new("fixture-window"),
            application: ApplicationIdentity {
                process_id: Some(1),
                executable_name: Some("fixture.exe".into()),
                executable_path: None,
                executable_hash: None,
                process_architecture: ProcessArchitecture::Unknown,
                top_level_window_class: Some("Fixture".into()),
                framework_hints: vec![FrameworkHint::Win32],
                version: None,
            },
            framework_hint: FrameworkHint::Win32,
            observation: CapabilityObservationProfile {
                window: supported.clone(),
                pixel: supported.clone(),
                semantic: supported.clone(),
                frame: supported.clone(),
            },
            input: CapabilityInputProfile {
                pointer: supported.clone(),
                keyboard: supported.clone(),
                text: supported.clone(),
            },
            semantic_actions: CapabilitySemanticActionProfile {
                focus: supported.clone(),
                invoke: supported.clone(),
                set_value: supported.clone(),
                toggle: supported.clone(),
                select: supported.clone(),
                expand_collapse: supported.clone(),
                range_value: supported.clone(),
                scroll_into_view: supported.clone(),
            },
            execution: CapabilityExecutionProfile {
                semantic_preferred: supported.clone(),
                pixel_fallback_available: supported,
            },
            environment: CapabilityEnvironmentProfile {
                access_boundary: ComputerAccessBoundary::Allowed,
                display_compatibility: unknown.clone(),
                topology_compatibility: unknown,
            },
            restrictions: CapabilityRestrictions {
                elevation_required: false,
                foreground_required: true,
                reasons: Vec::new(),
            },
            security: None,
            semantic_generation: Some(1),
            cache_hit: false,
            timings: CapabilityProbeTimings::default(),
            known_gaps: Vec::new(),
        };
        profile.apply_security_restriction("elevation_required");
        assert_eq!(profile.input.keyboard.status, CapabilityStatus::Restricted);
        assert_eq!(
            profile.semantic_actions.invoke.status,
            CapabilityStatus::Restricted
        );
        assert_eq!(
            profile.observation.pixel.status,
            CapabilityStatus::Supported
        );
        assert_eq!(
            profile.input.keyboard.provenance.source,
            CapabilitySource::SecurityPolicy
        );
    }
}
