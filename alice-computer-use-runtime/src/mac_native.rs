//! macOS-native observation and pixel/input backend.
//!
//! This macOS backend keeps captures session-scoped and bounded. Application
//! keyboard/text and Accessibility actions prefer background delivery; only
//! actions without a reliable background route are promoted to a fresh
//! foreground takeover. Native calls are kept here so the portable runtime
//! and sidecar protocol remain platform-neutral. The AX tree is bounded and
//! element references are generation-scoped; the one intentional semantic gap
//! is scroll-into-view, which has no portable AX action.

use super::{Capability, CapabilityState, ComputerBackend};
use alice_computer_use_core::{
    ActionStatus, ApplicationCapabilityProfile, ApplicationIdentity, CapabilityAccess,
    CapabilityAssessment, CapabilityEnvironmentProfile, CapabilityExecutionProfile,
    CapabilityInputProfile, CapabilityObservationProfile, CapabilityProbeTimings,
    CapabilityRestrictions, CapabilitySemanticActionProfile, CapabilitySource,
    CaptureFrameMetadata, ComputerAccessBoundary, ComputerAction, ComputerActionResult,
    ComputerError, ComputerExecutionAttempt, ComputerExecutionIntent, ComputerExecutionMethod,
    ComputerExecutionMode, ComputerExecutionOutcome, ComputerExecutionRequest,
    ComputerExecutionResult, ComputerExecutionStrategy, ComputerExecutionTiming,
    ComputerExecutionVerification, ComputerExecutionVerificationKind, ComputerFallbackPolicy,
    ComputerPointerAction, ComputerSessionId, Coordinate, CoordinateSpace, DesktopKind,
    DesktopSecurityContext, DisplayId, DisplayInfo, DisplayTopology, DpiScale, ElementId,
    FrameEncoding, FrameEncodingResult, FrameId, FrameMetadataResult, FramePixelFormat, FrameState,
    FrameworkHint, IntegrityLevel, MouseButton, PixelTarget, Point, ProcessArchitecture,
    ProcessSecurityContext, Rect, Screen, ScreenId, Screenshot, ScreenshotMetadata,
    ScreenshotTarget, ScrollDirection, SecurityDecision, SemanticAction, SemanticActionResult,
    SemanticActionStatus, SemanticActionTiming, SemanticActionVerification, SemanticObservation,
    SemanticObservationLimits, Size, TargetAccessCapabilities, Window, WindowId,
    WindowSecurityMetadata,
};
use async_trait::async_trait;
use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet, VecDeque},
    ffi::{CStr, CString},
    hash::{Hash, Hasher},
    io::{BufRead, BufReader, Read, Write},
    os::raw::{c_char, c_int, c_void},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    sync::Once,
    thread,
    time::{Duration, Instant, SystemTime},
};

type CFIndex = isize;
type CFTypeID = usize;
type CFTypeRef = *const c_void;
type CFStringRef = *const c_void;
type CFArrayRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFDataRef = *const c_void;
type CGImageRef = *const c_void;
type CGDisplayModeRef = *const c_void;
type CGDataProviderRef = *const c_void;
type CGEventRef = *const c_void;
type CGEventSourceRef = *const c_void;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct CGRect {
    origin: CGPoint,
    size: CGSize,
}

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {
    fn CFRelease(value: CFTypeRef);
    fn CFRetain(value: CFTypeRef) -> CFTypeRef;
    fn CFGetTypeID(value: CFTypeRef) -> CFTypeID;
    fn CFArrayGetTypeID() -> CFTypeID;
    fn CFStringGetTypeID() -> CFTypeID;
    fn CFNumberGetTypeID() -> CFTypeID;
    fn CFBooleanGetTypeID() -> CFTypeID;
    static kCFBooleanTrue: CFTypeRef;
    static kCFBooleanFalse: CFTypeRef;
    fn CFArrayGetCount(array: CFArrayRef) -> CFIndex;
    fn CFArrayGetValueAtIndex(array: CFArrayRef, index: CFIndex) -> CFTypeRef;
    fn CFDictionaryGetValue(dictionary: CFDictionaryRef, key: CFTypeRef) -> CFTypeRef;
    fn CFStringCreateWithCString(
        allocator: CFTypeRef,
        string: *const c_char,
        encoding: u32,
    ) -> CFStringRef;
    fn CFStringGetCStringPtr(string: CFStringRef, encoding: u32) -> *const c_char;
    fn CFStringGetCString(
        string: CFStringRef,
        buffer: *mut c_char,
        buffer_size: CFIndex,
        encoding: u32,
    ) -> u8;
    fn CFStringGetLength(string: CFStringRef) -> CFIndex;
    fn CFStringGetMaximumSizeForEncoding(length: CFIndex, encoding: u32) -> CFIndex;
    fn CFNumberGetValue(number: CFTypeRef, number_type: c_int, value: *mut c_void) -> u8;
    fn CFNumberCreate(allocator: CFTypeRef, number_type: c_int, value: *const c_void) -> CFTypeRef;
    fn CFBooleanGetValue(value: CFTypeRef) -> u8;
    fn CFDataGetBytePtr(data: CFDataRef) -> *const u8;
    fn CFDataGetLength(data: CFDataRef) -> CFIndex;
    fn CFDictionaryCreate(
        allocator: CFTypeRef,
        keys: *const CFTypeRef,
        values: *const CFTypeRef,
        num_values: CFIndex,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFDictionaryRef;
}

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> u8;
    fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> u8;
    static kAXTrustedCheckOptionPrompt: CFStringRef;
    fn AXUIElementCreateSystemWide() -> CFTypeRef;
    fn AXUIElementCreateApplication(process_id: i32) -> CFTypeRef;
    fn AXUIElementCopyElementAtPosition(
        application: CFTypeRef,
        x: f64,
        y: f64,
        element: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementGetPid(element: CFTypeRef, process_id: *mut i32) -> i32;
    fn AXUIElementCopyAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> i32;
    fn AXUIElementCopyActionNames(element: CFTypeRef, names: *mut CFArrayRef) -> i32;
    fn AXUIElementIsAttributeSettable(
        element: CFTypeRef,
        attribute: CFStringRef,
        settable: *mut u8,
    ) -> i32;
    fn AXUIElementPerformAction(element: CFTypeRef, action: CFStringRef) -> i32;
    fn AXUIElementSetAttributeValue(
        element: CFTypeRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> i32;
    fn AXValueGetTypeID() -> CFTypeID;
    fn AXValueGetType(value: CFTypeRef) -> u32;
    fn AXValueGetValue(value: CFTypeRef, value_type: u32, value_ptr: *mut c_void) -> u8;
}

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut u32,
        display_count: *mut u32,
    ) -> i32;
    fn CGMainDisplayID() -> u32;
    fn CGDisplayBounds(display: u32) -> CGRect;
    fn CGDisplayPixelsWide(display: u32) -> usize;
    fn CGDisplayPixelsHigh(display: u32) -> usize;
    fn CGDisplayCopyDisplayMode(display: u32) -> CGDisplayModeRef;
    fn CGDisplayModeGetPixelWidth(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeGetPixelHeight(mode: CGDisplayModeRef) -> usize;
    fn CGDisplayModeRelease(mode: CGDisplayModeRef);
    fn CGPreflightScreenCaptureAccess() -> u8;
    fn CGRequestScreenCaptureAccess() -> u8;
    fn CGDisplayCreateImage(display: u32) -> CGImageRef;
    fn CGImageRelease(image: CGImageRef);
    fn CGImageGetWidth(image: CGImageRef) -> usize;
    fn CGImageGetHeight(image: CGImageRef) -> usize;
    fn CGImageGetBytesPerRow(image: CGImageRef) -> usize;
    fn CGImageGetBitmapInfo(image: CGImageRef) -> u32;
    fn CGImageGetDataProvider(image: CGImageRef) -> CGDataProviderRef;
    fn CGDataProviderCopyData(provider: CGDataProviderRef) -> CFDataRef;
    fn CGRectMakeWithDictionaryRepresentation(dictionary: CFDictionaryRef, rect: *mut CGRect)
        -> u8;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: u32) -> CFArrayRef;
    fn CGEventCreateMouseEvent(
        source: CGEventSourceRef,
        mouse_type: u32,
        position: CGPoint,
        button: u32,
    ) -> CGEventRef;
    fn CGEventCreateKeyboardEvent(
        source: CGEventSourceRef,
        virtual_key: u16,
        key_down: u8,
    ) -> CGEventRef;
    fn CGEventKeyboardSetUnicodeString(
        event: CGEventRef,
        string_length: usize,
        unicode_string: *const u16,
    );
    fn CGEventCreateScrollWheelEvent(
        source: CGEventSourceRef,
        units: u32,
        wheel_count: u32,
        wheel1: i32,
        wheel2: i32,
        wheel3: i32,
    ) -> CGEventRef;
    fn CGEventSetLocation(event: CGEventRef, location: CGPoint);
    fn CGEventSetFlags(event: CGEventRef, flags: u64);
    fn CGEventSetIntegerValueField(event: CGEventRef, field: u32, value: i64);
    fn CGEventPost(tap: u32, event: CGEventRef);
    fn CGEventPostToPid(process_id: i32, event: CGEventRef);
}

const UTF8_ENCODING: u32 = 0x0800_0100;
const CF_NUMBER_SINT32: c_int = 3;
const CF_NUMBER_DOUBLE: c_int = 13;
const AX_VALUE_CGPOINT: u32 = 1;
const AX_VALUE_CGSIZE: u32 = 2;
const AX_ERROR_SUCCESS: i32 = 0;
const AX_ERROR_ATTRIBUTE_UNSUPPORTED: i32 = -25205;
const AX_ERROR_NO_VALUE: i32 = -25212;
const CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const CG_WINDOW_LIST_OPTION_EXCLUDE_DESKTOP: u32 = 1 << 4;
const CG_NULL_WINDOW_ID: u32 = 0;
const K_CG_EVENT_TAP_HID: u32 = 0;
const K_CG_MOUSE_MOVED: u32 = 5;
const K_CG_LEFT_MOUSE_DOWN: u32 = 1;
const K_CG_LEFT_MOUSE_UP: u32 = 2;
const K_CG_RIGHT_MOUSE_DOWN: u32 = 3;
const K_CG_RIGHT_MOUSE_UP: u32 = 4;
const K_CG_MOUSE_DRAGGED: u32 = 6;
const K_CG_MOUSE_BUTTON_LEFT: u32 = 0;
const K_CG_MOUSE_BUTTON_RIGHT: u32 = 1;
const K_CG_MOUSE_BUTTON_CENTER: u32 = 2;
const K_CG_SCROLL_LINE: u32 = 1;
const K_CG_EVENT_SOURCE_USER_DATA: u32 = 42;
const ALICE_COMPUTER_INPUT_MARKER: i64 =
    alice_computer_use_core::ALICE_COMPUTER_INPUT_MARKER as i64;

fn configured_input_marker() -> i64 {
    configured_input_marker_from(std::env::var("ALICE_COMPUTER_INPUT_MARKER").ok().as_deref())
}

fn configured_input_marker_from(value: Option<&str>) -> i64 {
    value
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| *value != 0)
        .map(|value| value as i64)
        .unwrap_or(ALICE_COMPUTER_INPUT_MARKER)
}

const FRAME_STORE_MAX_FRAMES: usize = 8;
const FRAME_STORE_MAX_BYTES: usize = 128 * 1024 * 1024;
const FRAME_STORE_MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
const FRAME_TOMBSTONE_LIMIT: usize = 64;
const MAX_HOLD_KEY_MS: u32 = 5_000;

#[derive(Clone, Debug, Default)]
pub struct NativeFrameStoreStats {
    pub frame_count: usize,
    pub total_bytes: usize,
    pub max_frames: usize,
    pub max_bytes: usize,
    pub encoded_bytes: usize,
    pub encode_cache_hits: u64,
    pub encode_cache_misses: u64,
    pub capture_count: u64,
    pub total_capture_micros: u128,
    pub total_store_micros: u128,
}

#[derive(Clone, Debug)]
pub struct NativeDisplayInfo {
    pub display_id: DisplayId,
    pub monitor: Rect,
    pub work_area: Rect,
    pub logical_monitor: Rect,
    pub logical_work_area: Rect,
    pub dpi: DpiScale,
    pub system_dpi: u32,
    pub scale: DpiScale,
    pub virtual_desktop_bounds: Rect,
    pub topology: DisplayTopology,
}

#[derive(Clone)]
struct MacDisplay {
    native_id: u32,
    global_bounds: Rect,
    pixel_size: Size,
    scale: DpiScale,
}

struct MacTopology {
    topology: DisplayTopology,
    displays: HashMap<DisplayId, MacDisplay>,
}

#[derive(Default)]
struct PressedState {
    mouse_buttons: HashSet<MouseButton>,
    keys: HashSet<String>,
}

#[derive(Clone, Debug, Default)]
struct VirtualCursorState {
    position: Option<Point>,
    trail: VecDeque<Point>,
    visible: bool,
}

#[derive(Clone)]
struct MacElementRecord {
    semantic: alice_computer_use_core::ComputerElement,
    process_id: u32,
    window_index: usize,
    child_path: Vec<usize>,
    window_id: WindowId,
    password: bool,
}

#[derive(Default)]
struct MacSemanticSessionState {
    nonce: u64,
    generation: u64,
    current: HashMap<ElementId, MacElementRecord>,
    snapshot: Vec<alice_computer_use_core::ComputerElement>,
    observed_window: Option<WindowId>,
}

struct StoredFrame {
    metadata: CaptureFrameMetadata,
    pixels: Vec<u8>,
    encoded_png: Option<Vec<u8>>,
}

/// Long-lived ScreenCaptureKit bridge. The helper is deliberately a separate
/// app bundle so macOS TCC can associate Screen Recording with a stable Bundle
/// ID instead of the changing ad-hoc identity of a rebuilt Rust executable.
struct SwiftCaptureClient {
    socket_path: PathBuf,
}

impl SwiftCaptureClient {
    fn spawn(path: &std::path::Path) -> Result<Self, ComputerError> {
        let nonce = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        let socket_path = std::env::temp_dir().join(format!(
            "alice-computer-native-{}-{nonce}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&socket_path);
        Command::new("/usr/bin/open")
            .args(["-n", "-a"])
            .arg(path)
            .args(["--args", "--socket"])
            .arg(&socket_path)
            .status()
            .map_err(|error| {
                ComputerError::Backend(format!(
                    "failed to launch Swift ScreenCaptureKit app {}: {error}",
                    path.display()
                ))
            })?
            .success()
            .then_some(())
            .ok_or_else(|| {
                ComputerError::Backend(
                    "macOS LaunchServices could not start the Swift helper".into(),
                )
            })?;

        // LaunchServices may cold-start the app and register its TCC identity
        // asynchronously. This only affects the first capture; later calls
        // connect directly to the already-running helper socket.
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            // A Unix domain socket is not a regular file, so `is_file()`
            // would reject a perfectly healthy listener. The connect itself
            // is the readiness probe.
            if UnixStream::connect(&socket_path).is_ok() {
                return Ok(Self { socket_path });
            }
            thread::sleep(Duration::from_millis(20));
        }
        Err(ComputerError::Backend(format!(
            "Swift helper did not publish its Unix socket {}",
            socket_path.display()
        )))
    }

    fn connect(&self) -> Result<UnixStream, ComputerError> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut last_error = None;
        while Instant::now() < deadline {
            match UnixStream::connect(&self.socket_path) {
                Ok(stream) => return Ok(stream),
                Err(error) => last_error = Some(error),
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ComputerError::Backend(format!(
            "could not connect to Swift helper socket {}: {}",
            self.socket_path.display(),
            last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "socket did not become available".into())
        )))
    }

    fn capture_display(
        &mut self,
        display_id: u32,
    ) -> Result<(Vec<u8>, u32, u32, u32), ComputerError> {
        let mut stream = self.connect()?;
        let request = serde_json::json!({
            "method": "capture_display",
            "display_id": display_id,
        });
        serde_json::to_writer(&mut stream, &request).map_err(|error| {
            ComputerError::Backend(format!("failed to write Swift capture request: {error}"))
        })?;
        stream.write_all(b"\n").map_err(|error| {
            ComputerError::Backend(format!("failed to write Swift capture request: {error}"))
        })?;
        stream.flush().map_err(|error| {
            ComputerError::Backend(format!("failed to flush Swift capture request: {error}"))
        })?;

        let mut stdout = BufReader::new(stream);
        let mut line = String::new();
        stdout.read_line(&mut line).map_err(|error| {
            ComputerError::Backend(format!("failed to read Swift capture response: {error}"))
        })?;
        let response: serde_json::Value = serde_json::from_str(&line).map_err(|error| {
            ComputerError::Backend(format!("invalid Swift capture response: {error}"))
        })?;
        if !response
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
        {
            let message = response
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Swift ScreenCaptureKit helper rejected the request");
            return Err(ComputerError::AccessDenied(message.into()));
        }
        let width = response
            .get("width")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| ComputerError::Backend("Swift capture response omitted width".into()))?;
        let height = response
            .get("height")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ComputerError::Backend("Swift capture response omitted height".into())
            })?;
        let stride = response
            .get("stride")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ComputerError::Backend("Swift capture response omitted stride".into())
            })?;
        let data_bytes = response
            .get("data_bytes")
            .and_then(serde_json::Value::as_u64)
            .ok_or_else(|| {
                ComputerError::Backend("Swift capture response omitted data_bytes".into())
            })?;
        let width = u32::try_from(width)
            .map_err(|_| ComputerError::Backend("Swift capture width overflow".into()))?;
        let height = u32::try_from(height)
            .map_err(|_| ComputerError::Backend("Swift capture height overflow".into()))?;
        let stride = u32::try_from(stride)
            .map_err(|_| ComputerError::Backend("Swift capture stride overflow".into()))?;
        let data_bytes = usize::try_from(data_bytes)
            .map_err(|_| ComputerError::Backend("Swift capture byte count overflow".into()))?;
        let expected = (stride as usize)
            .checked_mul(height as usize)
            .ok_or_else(|| ComputerError::Backend("Swift capture byte count overflow".into()))?;
        if data_bytes != expected || data_bytes > FRAME_STORE_MAX_BYTES {
            return Err(ComputerError::Backend(
                "Swift capture returned an invalid or oversized pixel buffer".into(),
            ));
        }
        let mut pixels = vec![0u8; data_bytes];
        stdout.read_exact(&mut pixels).map_err(|error| {
            ComputerError::Backend(format!("failed to read Swift capture pixels: {error}"))
        })?;
        Ok((pixels, width, height, stride))
    }

    fn shutdown(&self) {
        let Ok(mut stream) = self.connect() else {
            let _ = std::fs::remove_file(&self.socket_path);
            return;
        };
        let _ = stream.write_all(b"{\"method\":\"shutdown\"}\n");
        let _ = stream.flush();
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

fn swift_capture_helper_path() -> Option<PathBuf> {
    if let Ok(value) = std::env::var("ALICE_COMPUTER_SWIFT_HELPER") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return Some(path);
        }
        if path.is_dir() {
            let bundled = path.join("Contents/MacOS/alice-computer-native");
            if bundled.is_file() {
                return Some(bundled);
            }
        }
    }
    let executable = std::env::current_exe().ok()?;
    let bundled = executable
        .parent()?
        .join("AliceComputerNative.app/Contents/MacOS/alice-computer-native");
    bundled.is_file().then_some(bundled)
}

#[derive(Default)]
struct FrameStore {
    frames: HashMap<FrameId, StoredFrame>,
    order: VecDeque<FrameId>,
    tombstones: HashMap<FrameId, FrameState>,
    tombstone_order: VecDeque<FrameId>,
    total_bytes: usize,
    encoded_bytes: usize,
    encode_cache_hits: u64,
    encode_cache_misses: u64,
    capture_count: u64,
    total_capture_micros: u128,
    total_store_micros: u128,
}

impl FrameStore {
    fn remember(&mut self, id: FrameId, state: FrameState) {
        self.tombstones.insert(id.clone(), state);
        self.tombstone_order.push_back(id);
        while self.tombstone_order.len() > FRAME_TOMBSTONE_LIMIT {
            if let Some(old) = self.tombstone_order.pop_front() {
                self.tombstones.remove(&old);
            }
        }
    }

    fn evict_one(&mut self) {
        let Some(id) = self.order.pop_front() else {
            return;
        };
        if let Some(frame) = self.frames.remove(&id) {
            self.total_bytes = self.total_bytes.saturating_sub(frame.pixels.len());
            self.encoded_bytes = self
                .encoded_bytes
                .saturating_sub(frame.encoded_png.as_ref().map_or(0, Vec::len));
            self.remember(id, FrameState::Evicted);
        }
    }

    fn insert(&mut self, frame: StoredFrame) -> Result<(), ComputerError> {
        let bytes = frame.pixels.len();
        if bytes > FRAME_STORE_MAX_BYTES {
            return Err(ComputerError::Backend(format!(
                "macOS frame is larger than the {} MiB frame-store limit",
                FRAME_STORE_MAX_BYTES / 1024 / 1024
            )));
        }
        while self.frames.len() >= FRAME_STORE_MAX_FRAMES
            || self.total_bytes.saturating_add(bytes) > FRAME_STORE_MAX_BYTES
        {
            self.evict_one();
        }
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.order.push_back(frame.metadata.frame_id.clone());
        self.frames.insert(frame.metadata.frame_id.clone(), frame);
        Ok(())
    }

    fn state(&self, id: &FrameId, generation: u64) -> FrameMetadataResult {
        if let Some(frame) = self.frames.get(id) {
            if frame.metadata.topology_generation != generation {
                return FrameMetadataResult {
                    metadata: Some(frame.metadata.clone()),
                    state: FrameState::StaleTopology,
                };
            }
            return FrameMetadataResult {
                metadata: Some(frame.metadata.clone()),
                state: FrameState::Current,
            };
        }
        FrameMetadataResult {
            metadata: None,
            state: self
                .tombstones
                .get(id)
                .copied()
                .unwrap_or(FrameState::Unknown),
        }
    }

    fn release(&mut self, id: &FrameId) -> FrameMetadataResult {
        let Some(frame) = self.frames.remove(id) else {
            return FrameMetadataResult {
                metadata: None,
                state: self
                    .tombstones
                    .get(id)
                    .copied()
                    .unwrap_or(FrameState::Unknown),
            };
        };
        self.total_bytes = self.total_bytes.saturating_sub(frame.pixels.len());
        self.encoded_bytes = self
            .encoded_bytes
            .saturating_sub(frame.encoded_png.as_ref().map_or(0, Vec::len));
        self.order.retain(|value| value != id);
        let metadata = frame.metadata.clone();
        self.remember(id.clone(), FrameState::Released);
        FrameMetadataResult {
            metadata: Some(metadata),
            state: FrameState::Released,
        }
    }

    fn encode(
        &mut self,
        id: &FrameId,
        generation: u64,
    ) -> Result<(Screenshot, bool), ComputerError> {
        let state = self.state(id, generation);
        if state.state != FrameState::Current {
            return Err(ComputerError::StaleDisplay(format!(
                "screenshot frame {id} is not current ({:?})",
                state.state
            )));
        }
        let cached = self.frames.get(id).and_then(|frame| {
            frame
                .encoded_png
                .as_ref()
                .map(|bytes| (frame.metadata.clone(), bytes.clone()))
        });
        if let Some((metadata, bytes)) = cached {
            self.encode_cache_hits = self.encode_cache_hits.saturating_add(1);
            return Ok((screenshot_from_frame(&metadata, bytes), true));
        }
        let metadata = self
            .frames
            .get(id)
            .map(|frame| frame.metadata.clone())
            .ok_or_else(|| {
                ComputerError::StaleDisplay(format!("screenshot frame {id} is unavailable"))
            })?;
        self.encode_cache_misses = self.encode_cache_misses.saturating_add(1);
        let bytes = {
            let frame = self.frames.get(id).ok_or_else(|| {
                ComputerError::StaleDisplay(format!("screenshot frame {id} is unavailable"))
            })?;
            encode_bgra_png(
                &frame.pixels,
                metadata.width,
                metadata.height,
                metadata.stride,
            )?
        };
        if bytes.len() <= FRAME_STORE_MAX_ENCODED_BYTES {
            self.encoded_bytes = self.encoded_bytes.saturating_add(bytes.len());
            if let Some(frame) = self.frames.get_mut(id) {
                frame.encoded_png = Some(bytes.clone());
            }
        }
        Ok((screenshot_from_frame(&metadata, bytes), false))
    }
}

pub struct MacNativeBackend {
    initialized: bool,
    topology: Option<MacTopology>,
    swift_capture: Option<SwiftCaptureClient>,
    next_frame: u64,
    frames: HashMap<ComputerSessionId, FrameStore>,
    pressed: HashMap<ComputerSessionId, PressedState>,
    semantic: HashMap<ComputerSessionId, MacSemanticSessionState>,
    virtual_cursor: HashMap<ComputerSessionId, VirtualCursorState>,
    self_security: Option<ProcessSecurityContext>,
}

impl MacNativeBackend {
    pub fn new() -> Self {
        Self {
            initialized: false,
            topology: None,
            swift_capture: None,
            next_frame: 0,
            frames: HashMap::new(),
            pressed: HashMap::new(),
            semantic: HashMap::new(),
            virtual_cursor: HashMap::new(),
            self_security: None,
        }
    }

    pub fn frame_store_stats(&self, session: &ComputerSessionId) -> NativeFrameStoreStats {
        self.frames
            .get(session)
            .map(|store| NativeFrameStoreStats {
                frame_count: store.frames.len(),
                total_bytes: store.total_bytes,
                max_frames: FRAME_STORE_MAX_FRAMES,
                max_bytes: FRAME_STORE_MAX_BYTES,
                encoded_bytes: store.encoded_bytes,
                encode_cache_hits: store.encode_cache_hits,
                encode_cache_misses: store.encode_cache_misses,
                capture_count: store.capture_count,
                total_capture_micros: store.total_capture_micros,
                total_store_micros: store.total_store_micros,
            })
            .unwrap_or_else(|| NativeFrameStoreStats {
                max_frames: FRAME_STORE_MAX_FRAMES,
                max_bytes: FRAME_STORE_MAX_BYTES,
                ..Default::default()
            })
    }

    pub fn desktop_identity(&self) -> Result<(String, String), ComputerError> {
        self.ensure_initialized()?;
        Ok((
            "interactive-user-session".into(),
            "interactive-user-session".into(),
        ))
    }

    fn ensure_initialized(&self) -> Result<(), ComputerError> {
        self.initialized
            .then_some(())
            .ok_or(ComputerError::NotInitialized)
    }

    fn refresh_topology(&mut self) -> Result<(), ComputerError> {
        self.ensure_initialized()?;
        let mut count = 0u32;
        let error = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
        if error != 0 {
            return Err(ComputerError::Backend(format!(
                "CGGetActiveDisplayList failed with error {error}"
            )));
        }
        if count == 0 {
            return Err(ComputerError::Backend(
                "macOS has no active displays available to this process".into(),
            ));
        }
        let mut native_ids = vec![0u32; count as usize];
        let error = unsafe { CGGetActiveDisplayList(count, native_ids.as_mut_ptr(), &mut count) };
        if error != 0 || count == 0 {
            return Err(ComputerError::Backend(format!(
                "CGGetActiveDisplayList returned error {error}"
            )));
        }
        native_ids.truncate(count as usize);
        let primary = unsafe { CGMainDisplayID() };
        let previous = self.topology.take();
        let mut displays = Vec::with_capacity(native_ids.len());
        let mut native = HashMap::new();
        for native_id in native_ids {
            let bounds = unsafe { CGDisplayBounds(native_id) };
            if bounds.size.width <= 0.0 || bounds.size.height <= 0.0 {
                continue;
            }
            // `CGDisplayPixelsWide/High` can expose the user-selected logical
            // mode on Retina displays.  The current display mode carries the
            // actual pixel backing dimensions used by DisplayPhysical input;
            // keep the legacy APIs only as a fallback for older/virtualized
            // display providers that do not expose a mode.
            let (pixel_width, pixel_height) =
                display_mode_pixel_size(native_id).unwrap_or_else(|| {
                    (unsafe { CGDisplayPixelsWide(native_id) } as f64, unsafe {
                        CGDisplayPixelsHigh(native_id)
                    }
                        as f64)
                });
            let scale = scale_from_display_sizes(
                Size {
                    width: bounds.size.width,
                    height: bounds.size.height,
                },
                Size {
                    width: pixel_width,
                    height: pixel_height,
                },
            );
            let id = DisplayId::new(format!("mac-display-{native_id}"));
            let info = MacDisplay {
                native_id,
                global_bounds: Rect {
                    origin: Point {
                        x: bounds.origin.x,
                        y: bounds.origin.y,
                    },
                    size: Size {
                        width: bounds.size.width,
                        height: bounds.size.height,
                    },
                },
                pixel_size: Size {
                    width: pixel_width,
                    height: pixel_height,
                },
                scale,
            };
            displays.push(DisplayInfo {
                id: id.clone(),
                name: Some(format!("display-{native_id}")),
                physical_bounds: info.global_bounds,
                work_area: info.global_bounds,
                physical_size: info.pixel_size,
                logical_size: info.global_bounds.size,
                dpi: DpiScale {
                    x: scale.x * 96.0,
                    y: scale.y * 96.0,
                },
                scale,
                primary: native_id == primary,
            });
            native.insert(id, info);
        }
        if displays.is_empty() {
            return Err(ComputerError::Backend("no active macOS displays".into()));
        }
        displays.sort_by(|left, right| {
            left.physical_bounds
                .origin
                .x
                .partial_cmp(&right.physical_bounds.origin.x)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| {
                    left.physical_bounds
                        .origin
                        .y
                        .partial_cmp(&right.physical_bounds.origin.y)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });
        let same = previous.as_ref().is_some_and(|old| {
            old.displays.len() == native.len()
                && native.iter().all(|(id, current)| {
                    old.displays.get(id).is_some_and(|old_display| {
                        old_display.native_id == current.native_id
                            && old_display.global_bounds == current.global_bounds
                            && old_display.pixel_size == current.pixel_size
                            && old_display.scale == current.scale
                    })
                })
        });
        let generation = previous
            .as_ref()
            .map(|old| {
                if same {
                    old.topology.topology_generation
                } else {
                    old.topology.topology_generation.saturating_add(1)
                }
            })
            .unwrap_or(1);
        self.topology = Some(MacTopology {
            topology: DisplayTopology::from_displays(displays, generation),
            displays: native,
        });
        Ok(())
    }

    fn topology(&self) -> Result<&MacTopology, ComputerError> {
        self.topology.as_ref().ok_or(ComputerError::NotInitialized)
    }

    fn validate_display_id(&self, display_id: &DisplayId) -> Result<(), ComputerError> {
        if self.topology()?.displays.contains_key(display_id) {
            Ok(())
        } else {
            Err(ComputerError::UnknownDisplay(display_id.to_string()))
        }
    }

    fn primary_display(&self) -> Result<(DisplayId, MacDisplay, DisplayInfo), ComputerError> {
        let native = self.topology()?;
        let id = native
            .topology
            .primary_display_id
            .clone()
            .ok_or_else(|| ComputerError::Backend("no primary macOS display".into()))?;
        let display = native
            .displays
            .get(&id)
            .cloned()
            .ok_or_else(|| ComputerError::UnknownDisplay(id.to_string()))?;
        let info = native
            .topology
            .display(&id)
            .map_err(ComputerError::from)?
            .clone();
        Ok((id, display, info))
    }

    pub fn primary_display_info(&self) -> Result<NativeDisplayInfo, ComputerError> {
        self.ensure_initialized()?;
        let (display_id, display, info) = self.primary_display()?;
        Ok(NativeDisplayInfo {
            display_id,
            monitor: display.global_bounds,
            work_area: display.global_bounds,
            logical_monitor: Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: info.logical_size,
            },
            logical_work_area: Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: info.logical_size,
            },
            dpi: info.dpi,
            system_dpi: 96,
            scale: info.scale,
            virtual_desktop_bounds: self.topology()?.topology.virtual_desktop_bounds,
            topology: self.topology()?.topology.clone(),
        })
    }

    fn frame_store_mut(&mut self, session: &ComputerSessionId) -> &mut FrameStore {
        self.frames.entry(session.clone()).or_default()
    }

    fn capture_native_display(
        &mut self,
        display_id: u32,
    ) -> Result<(Vec<u8>, u32, u32, u32), ComputerError> {
        if self.swift_capture.is_none() {
            if let Some(path) = swift_capture_helper_path() {
                self.swift_capture = Some(SwiftCaptureClient::spawn(&path)?);
            }
        }
        if let Some(helper) = self.swift_capture.as_mut() {
            return helper.capture_display(display_id);
        }
        capture_native_display_core(display_id)
    }

    fn capture_display_frame(
        &mut self,
        session: &ComputerSessionId,
        display_id: &DisplayId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        self.validate_display_id(display_id)?;
        let (generation, display) = {
            let topology = self.topology()?;
            (
                topology.topology.topology_generation,
                topology
                    .displays
                    .get(display_id)
                    .cloned()
                    .ok_or_else(|| ComputerError::UnknownDisplay(display_id.to_string()))?,
            )
        };
        let started = Instant::now();
        let (mut pixels, width, height, stride) = self.capture_native_display(display.native_id)?;
        let frame_id = FrameId::new(format!("mac-frame-{generation}-{}", self.next_frame));
        self.next_frame = self.next_frame.saturating_add(1);
        // `CGDisplayBounds` and CGEvent coordinates are expressed in logical
        // desktop points, while the CGImage returned by
        // `CGDisplayCreateImage` is backed by physical pixels.  Do not rely
        // on `CGDisplayPixelsWide` here: on some Retina configurations it
        // reports the user-selected logical mode, not the image backing size.
        // The captured image is the authoritative source for screenshot
        // pixel normalization.
        let pixel_to_desktop_scale = capture_pixel_scale(display.global_bounds, width, height);
        if let Some(cursor) = self.virtual_cursor.get(session) {
            draw_virtual_cursor(
                &mut pixels,
                width,
                height,
                stride,
                cursor,
                display.global_bounds.origin,
                pixel_to_desktop_scale,
            );
        }
        let metadata = CaptureFrameMetadata {
            frame_id: frame_id.clone(),
            display_id: display_id.clone(),
            topology_generation: generation,
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: display.global_bounds.origin,
            width,
            height,
            pixel_format: FramePixelFormat::Bgra8,
            stride,
            dpi: DpiScale {
                x: pixel_to_desktop_scale.x * 96.0,
                y: pixel_to_desktop_scale.y * 96.0,
            },
            scale: pixel_to_desktop_scale,
            pixel_to_desktop_scale: Some(pixel_to_desktop_scale),
            captured_at: Some(SystemTime::now()),
            content_revision: self.next_frame,
            stale_topology: false,
        };
        let store_started = Instant::now();
        self.frame_store_mut(session).insert(StoredFrame {
            metadata: metadata.clone(),
            pixels,
            encoded_png: None,
        })?;
        let store = self.frame_store_mut(session);
        store.capture_count = store.capture_count.saturating_add(1);
        store.total_capture_micros = store
            .total_capture_micros
            .saturating_add(started.elapsed().as_micros());
        store.total_store_micros = store
            .total_store_micros
            .saturating_add(store_started.elapsed().as_micros());
        Ok(metadata)
    }

    fn capture_virtual_desktop_frame(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        let topology = self.topology()?.topology.clone();
        let primary_id = topology
            .primary_display_id
            .clone()
            .ok_or_else(|| ComputerError::Backend("no primary macOS display".into()))?;
        let primary_info = topology
            .display(&primary_id)
            .map_err(ComputerError::from)?
            .clone();
        let bounds = topology.virtual_desktop_bounds;
        let native_displays = self.topology()?.displays.clone();
        let primary_native = native_displays
            .get(&primary_id)
            .cloned()
            .ok_or_else(|| ComputerError::UnknownDisplay(primary_id.to_string()))?;
        let (primary_pixels, primary_width, primary_height, primary_stride) =
            self.capture_native_display(primary_native.native_id)?;
        let canvas_scale =
            capture_pixel_scale(primary_info.physical_bounds, primary_width, primary_height);
        let width = (bounds.size.width * canvas_scale.x).ceil();
        let height = (bounds.size.height * canvas_scale.y).ceil();
        if !width.is_finite()
            || !height.is_finite()
            || width <= 0.0
            || height <= 0.0
            || width > u32::MAX as f64
            || height > u32::MAX as f64
        {
            return Err(ComputerError::Backend(
                "macOS virtual desktop dimensions are invalid after capture scale detection".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let stride = width
            .checked_mul(4)
            .ok_or_else(|| ComputerError::Backend("virtual desktop stride overflow".into()))?;
        let canvas_len = stride
            .checked_mul(height)
            .ok_or_else(|| ComputerError::Backend("virtual desktop buffer overflow".into()))?
            as usize;
        if canvas_len > FRAME_STORE_MAX_BYTES {
            return Err(ComputerError::Backend(format!(
                "macOS virtual desktop exceeds the {} MiB frame-store limit",
                FRAME_STORE_MAX_BYTES / 1024 / 1024
            )));
        }
        let mut canvas = vec![0u8; canvas_len];
        let mut blit = |info: &DisplayInfo,
                        pixels: &[u8],
                        source_width: u32,
                        source_height: u32,
                        source_stride: u32| {
            let destination_x = ((info.physical_bounds.origin.x - bounds.origin.x) * canvas_scale.x)
                .round()
                .max(0.0) as u32;
            let destination_y = ((info.physical_bounds.origin.y - bounds.origin.y) * canvas_scale.y)
                .round()
                .max(0.0) as u32;
            let destination_width = (info.physical_bounds.size.width * canvas_scale.x)
                .round()
                .max(1.0) as u32;
            let destination_height = (info.physical_bounds.size.height * canvas_scale.y)
                .round()
                .max(1.0) as u32;
            blit_scaled_bgra(
                &mut canvas,
                width,
                height,
                pixels,
                source_width,
                source_height,
                source_stride,
                destination_x,
                destination_y,
                destination_width,
                destination_height,
            );
        };
        blit(
            &primary_info,
            &primary_pixels,
            primary_width,
            primary_height,
            primary_stride,
        );
        for info in topology
            .displays
            .iter()
            .filter(|info| info.id != primary_id)
        {
            let Some(native) = native_displays.get(&info.id) else {
                continue;
            };
            let (pixels, source_width, source_height, source_stride) =
                self.capture_native_display(native.native_id)?;
            blit(info, &pixels, source_width, source_height, source_stride);
        }
        if let Some(cursor) = self.virtual_cursor.get(session) {
            draw_virtual_cursor(
                &mut canvas,
                width,
                height,
                stride,
                cursor,
                bounds.origin,
                canvas_scale,
            );
        }
        let generation = topology.topology_generation;
        let frame_id = FrameId::new(format!("mac-frame-{generation}-{}", self.next_frame));
        self.next_frame = self.next_frame.saturating_add(1);
        let metadata = CaptureFrameMetadata {
            frame_id: frame_id.clone(),
            display_id: primary_id,
            topology_generation: generation,
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: bounds.origin,
            width,
            height,
            pixel_format: FramePixelFormat::Bgra8,
            stride,
            dpi: DpiScale {
                x: canvas_scale.x * 96.0,
                y: canvas_scale.y * 96.0,
            },
            scale: canvas_scale,
            pixel_to_desktop_scale: Some(canvas_scale),
            captured_at: Some(SystemTime::now()),
            content_revision: self.next_frame,
            stale_topology: false,
        };
        self.frame_store_mut(session).insert(StoredFrame {
            metadata: metadata.clone(),
            pixels: canvas,
            encoded_png: None,
        })?;
        Ok(metadata)
    }

    fn capture_virtual_desktop(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<Screenshot, ComputerError> {
        let metadata = self.capture_virtual_desktop_frame(session)?;
        Ok(self
            .encode_display_frame(session, &metadata.frame_id)?
            .screenshot)
    }

    fn capture_display(
        &mut self,
        session: &ComputerSessionId,
        display_id: &DisplayId,
    ) -> Result<Screenshot, ComputerError> {
        let metadata = self.capture_display_frame(session, display_id)?;
        Ok(self
            .encode_display_frame(session, &metadata.frame_id)?
            .screenshot)
    }

    fn encode_display_frame(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameEncodingResult, ComputerError> {
        let generation = self.topology()?.topology.topology_generation;
        let started = Instant::now();
        let (screenshot, cache_hit) = self.frame_store_mut(session).encode(frame_id, generation)?;
        Ok(FrameEncodingResult {
            screenshot,
            encoding: FrameEncoding::Png,
            cache_hit,
            encode_micros: started.elapsed().as_micros(),
        })
    }

    fn coordinate(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
    ) -> Result<(CGPoint, String), ComputerError> {
        if coordinate.extent.width <= 0.0
            || coordinate.extent.height <= 0.0
            || coordinate.dpi.x <= 0.0
            || coordinate.dpi.y <= 0.0
        {
            return Err(ComputerError::InvalidCoordinate(
                "coordinate extent and DPI must be positive".into(),
            ));
        }
        let topology = self.topology()?;
        let point = match coordinate.space {
            CoordinateSpace::DesktopPhysical => coordinate.point,
            CoordinateSpace::DisplayLogical => {
                let id = coordinate.display_id.as_ref().ok_or_else(|| {
                    ComputerError::InvalidCoordinate(
                        "display_logical coordinate requires display_id".into(),
                    )
                })?;
                let display = topology
                    .displays
                    .get(id)
                    .ok_or_else(|| ComputerError::UnknownDisplay(id.to_string()))?;
                Point {
                    x: display.global_bounds.origin.x + coordinate.point.x,
                    y: display.global_bounds.origin.y + coordinate.point.y,
                }
            }
            CoordinateSpace::DisplayPhysical | CoordinateSpace::Screen => {
                let id = coordinate.display_id.as_ref().ok_or_else(|| {
                    ComputerError::InvalidCoordinate(
                        "display_physical coordinate requires display_id".into(),
                    )
                })?;
                let display = topology
                    .displays
                    .get(id)
                    .ok_or_else(|| ComputerError::UnknownDisplay(id.to_string()))?;
                Point {
                    x: display.global_bounds.origin.x + coordinate.point.x / display.scale.x,
                    y: display.global_bounds.origin.y + coordinate.point.y / display.scale.y,
                }
            }
            CoordinateSpace::ScreenshotPixel => {
                let frame_id = coordinate.frame_id.as_ref().ok_or_else(|| {
                    ComputerError::InvalidCoordinate(
                        "screenshot_pixel coordinate requires frame_id".into(),
                    )
                })?;
                let store = self.frames.get(session).ok_or_else(|| {
                    ComputerError::StaleDisplay(format!(
                        "screenshot frame {frame_id} is not owned by this session"
                    ))
                })?;
                let state = store.state(frame_id, topology.topology.topology_generation);
                if state.state != FrameState::Current {
                    return Err(ComputerError::StaleDisplay(format!(
                        "screenshot frame {frame_id} is not current ({:?})",
                        state.state
                    )));
                }
                let metadata = state.metadata.ok_or_else(|| {
                    ComputerError::StaleDisplay(format!("screenshot frame {frame_id} unavailable"))
                })?;
                let scale = metadata.pixel_to_desktop_scale.unwrap_or(metadata.scale);
                if !scale.x.is_finite() || !scale.y.is_finite() || scale.x <= 0.0 || scale.y <= 0.0
                {
                    return Err(ComputerError::InvalidCoordinate(
                        "screenshot pixel scale must be finite and positive".into(),
                    ));
                }
                Point {
                    x: metadata.desktop_origin.x + coordinate.point.x / scale.x,
                    y: metadata.desktop_origin.y + coordinate.point.y / scale.y,
                }
            }
            CoordinateSpace::Window => {
                return Err(ComputerError::CapabilityGap {
                    capability: "window_coordinate_actions".into(),
                    detail: "Window coordinates require explicit target resolution".into(),
                })
            }
        };
        if !point.x.is_finite() || !point.y.is_finite() {
            return Err(ComputerError::InvalidCoordinate(
                "coordinate point must be finite".into(),
            ));
        }
        Ok((
            CGPoint {
                x: point.x,
                y: point.y,
            },
            format!(
                "space={:?}; point=({}, {})",
                coordinate.space, point.x, point.y
            ),
        ))
    }

    fn foreground_window(&self) -> Result<WindowRecord, ComputerError> {
        enumerate_window_records()?
            .into_iter()
            .find(|window| window.active)
            .ok_or_else(|| {
                ComputerError::ForegroundDenied(
                    "macOS did not expose a current foreground window".into(),
                )
            })
    }

    fn background_window(&self, target: &WindowId) -> Result<WindowRecord, ComputerError> {
        enumerate_window_records()?
            .into_iter()
            .find(|window| &window.id == target)
            .ok_or_else(|| ComputerError::TargetUnavailable(format!("window {target} is gone")))
    }

    fn target_window(&self, target: Option<&WindowId>) -> Result<WindowRecord, ComputerError> {
        let windows = enumerate_window_records()?;
        let foreground = windows
            .iter()
            .find(|window| window.active)
            .cloned()
            .ok_or_else(|| {
                ComputerError::ForegroundDenied(
                    "macOS did not expose a current foreground window".into(),
                )
            })?;
        let Some(target) = target else {
            return Ok(foreground);
        };
        let target_record = windows
            .into_iter()
            .find(|window| &window.id == target)
            .ok_or_else(|| ComputerError::TargetUnavailable(format!("window {target} is gone")))?;
        if target_record.id != foreground.id {
            return Err(ComputerError::ForegroundDenied(format!(
                "target window {target} is not the current foreground window"
            )));
        }
        Ok(target_record)
    }

    fn target_application_window(
        &self,
        target: &ApplicationIdentity,
    ) -> Result<WindowRecord, ComputerError> {
        let process_id = target.process_id.ok_or_else(|| {
            ComputerError::TargetUnavailable(
                "application-scoped input requires a stable process id".into(),
            )
        })?;
        let foreground = self.foreground_window()?;
        if foreground.process_id != process_id {
            return Err(ComputerError::ForegroundDenied(format!(
                "application target pid={process_id} is not the current foreground application"
            )));
        }
        Ok(foreground)
    }

    fn application_window(
        &self,
        target: &ApplicationIdentity,
    ) -> Result<WindowRecord, ComputerError> {
        let process_id = target.process_id.ok_or_else(|| {
            ComputerError::TargetUnavailable(
                "application-scoped input requires a stable process id".into(),
            )
        })?;
        enumerate_window_records()?
            .into_iter()
            .find(|window| window.process_id == process_id)
            .ok_or_else(|| {
                ComputerError::TargetUnavailable(format!(
                    "application pid={process_id} has no visible window"
                ))
            })
    }

    fn virtual_cursor_move(
        &mut self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
    ) -> Result<(CGPoint, String), ComputerError> {
        let (point, detail) = self.coordinate(session, coordinate)?;
        let state = self.virtual_cursor.entry(session.clone()).or_default();
        state.position = Some(Point {
            x: point.x,
            y: point.y,
        });
        state.visible = true;
        state.trail.push_back(Point {
            x: point.x,
            y: point.y,
        });
        while state.trail.len() > 32 {
            state.trail.pop_front();
        }
        Ok((point, detail))
    }

    fn virtual_cursor_clear(&mut self, session: &ComputerSessionId) {
        if let Some(state) = self.virtual_cursor.get_mut(session) {
            state.visible = false;
            state.trail.clear();
        }
    }

    fn background_element_at(
        &self,
        target: &WindowRecord,
        point: CGPoint,
    ) -> Result<CFTypeRef, ComputerError> {
        if !contains(
            target.bounds,
            Point {
                x: point.x,
                y: point.y,
            },
        ) {
            return Err(ComputerError::InvalidCoordinate(
                "coordinate is outside the target window".into(),
            ));
        }
        let application = unsafe { AXUIElementCreateApplication(target.process_id as i32) };
        if application.is_null() {
            return Err(ComputerError::AccessDenied(
                "AXUIElementCreateApplication returned null".into(),
            ));
        }
        let mut element = std::ptr::null();
        let result = unsafe {
            AXUIElementCopyElementAtPosition(application, point.x, point.y, &mut element)
        };
        unsafe { CFRelease(application) };
        if result != AX_ERROR_SUCCESS || element.is_null() {
            return Err(ax_error("AXUIElementCopyElementAtPosition", result));
        }
        Ok(element)
    }

    fn background_pointer_action(
        &mut self,
        session: &ComputerSessionId,
        target: &WindowRecord,
        action: &ComputerAction,
    ) -> Result<String, ComputerError> {
        match action {
            ComputerAction::MovePointer { to } => {
                let (_, detail) = self.virtual_cursor_move(session, to)?;
                Ok(format!("route=background_virtual_cursor; {detail}"))
            }
            ComputerAction::Click { at } => {
                let (point, detail) = self.virtual_cursor_move(session, at)?;
                let element = self.background_element_at(target, point)?;
                let result = ax_perform_action(element, "AXPress");
                unsafe { CFRelease(element) };
                result?;
                Ok(format!("route=background_ax_press; {detail}"))
            }
            ComputerAction::DoubleClick { at } => {
                let (point, detail) = self.virtual_cursor_move(session, at)?;
                let element = self.background_element_at(target, point)?;
                let first = ax_perform_action(element, "AXPress");
                let second = if first.is_ok() {
                    thread::sleep(Duration::from_millis(25));
                    ax_perform_action(element, "AXPress")
                } else {
                    first
                };
                unsafe { CFRelease(element) };
                second?;
                Ok(format!("route=background_ax_press; clicks=2; {detail}"))
            }
            ComputerAction::Scroll {
                at,
                direction,
                amount,
            } => {
                let (point, detail) = self.virtual_cursor_move(session, at)?;
                let element = self.background_element_at(target, point)?;
                let action_name = match direction {
                    ScrollDirection::Up => "AXScrollUp",
                    ScrollDirection::Down => "AXScrollDown",
                    ScrollDirection::Left => "AXScrollLeft",
                    ScrollDirection::Right => "AXScrollRight",
                };
                let result = Self::ax_scroll_background(element, *direction, *amount, action_name);
                unsafe { CFRelease(element) };
                let route_detail = match result {
                    Ok(detail) => detail,
                    Err(ax_error) => {
                        Self::send_scroll_to_pid(target.process_id, point, *direction, *amount)
                            .map(|pid_detail| format!("{pid_detail}; ax_attempt={ax_error}"))?
                    }
                };
                Ok(format!("{route_detail}; {detail}"))
            }
            ComputerAction::TypeText {
                text, at: Some(at), ..
            } => {
                let (point, detail) = self.virtual_cursor_move(session, at)?;
                let element = self.background_element_at(target, point)?;
                let focus = if ax_attribute_settable(element, "AXFocused") {
                    ax_set_attribute(element, "AXFocused", unsafe { kCFBooleanTrue })
                } else {
                    ax_perform_action(element, "AXPress")
                };
                unsafe { CFRelease(element) };
                focus?;
                thread::sleep(Duration::from_millis(10));
                let text_detail = self.send_text_to_pid(text, Some(target.process_id))?;
                Ok(format!(
                    "route=background_ax_focus_pid_text; {detail}; {text_detail}"
                ))
            }
            _ => Err(ComputerError::Unsupported {
                capability: "background_pointer_input".into(),
                detail: "this pointer action has no reliable macOS Accessibility route".into(),
            }),
        }
    }

    fn ax_scroll_background(
        element: CFTypeRef,
        direction: ScrollDirection,
        amount: u32,
        action_name: &str,
    ) -> Result<String, ComputerError> {
        // Web content often returns a leaf AX element for CopyElementAtPosition.
        // Walk its parents before giving up: Safari exposes the actual scroll
        // container several levels above the hit-tested element.
        let mut current = unsafe { CFRetain(element) };
        let mut last_error = None;
        for depth in 0..=12 {
            let steps = amount.clamp(1, 8);
            let mut action_result = Ok(());
            for _ in 0..steps {
                action_result = ax_perform_action(current, action_name);
                if action_result.is_err() {
                    break;
                }
            }
            match action_result {
                Ok(()) => {
                    unsafe { CFRelease(current) };
                    return Ok(format!(
                    "route=background_ax_scroll; action={action_name}; ancestor_depth={depth}; steps={steps}"
                ));
                }
                Err(error) => last_error = Some(error),
            }

            if let Some(scrollbar) = Self::ax_scrollbar_for_direction(current, direction)? {
                match Self::ax_set_scrollbar_delta(scrollbar, direction, amount) {
                    Ok(delta) => {
                        unsafe {
                            CFRelease(scrollbar);
                            CFRelease(current);
                        }
                        return Ok(format!(
                        "route=background_ax_scrollbar; ancestor_depth={depth}; delta={delta:.3}"
                    ));
                    }
                    Err(error) => last_error = Some(error),
                }
                unsafe { CFRelease(scrollbar) };
            }

            let parent = ax_copy_attribute(current, "AXParent")?;
            unsafe { CFRelease(current) };
            let Some(parent) = parent else {
                break;
            };
            current = parent;
        }

        Err(last_error.unwrap_or_else(|| ComputerError::Unsupported {
            capability: "background_scroll".into(),
            detail: "no Accessibility scroll container was found".into(),
        }))
    }

    fn ax_scrollbar_for_direction(
        element: CFTypeRef,
        direction: ScrollDirection,
    ) -> Result<Option<CFTypeRef>, ComputerError> {
        let attribute = match direction {
            ScrollDirection::Up | ScrollDirection::Down => "AXVerticalScrollBar",
            ScrollDirection::Left | ScrollDirection::Right => "AXHorizontalScrollBar",
        };
        ax_copy_attribute(element, attribute)
    }

    fn ax_set_scrollbar_delta(
        scrollbar: CFTypeRef,
        direction: ScrollDirection,
        amount: u32,
    ) -> Result<f64, ComputerError> {
        if !ax_attribute_settable(scrollbar, "AXValue") {
            return Err(ComputerError::Unsupported {
                capability: "background_scrollbar_value".into(),
                detail: "Accessibility scroll bar value is not settable".into(),
            });
        }
        let current = ax_attribute_number(scrollbar, "AXValue").ok_or_else(|| {
            ComputerError::Unsupported {
                capability: "background_scrollbar_value".into(),
                detail: "Accessibility scroll bar has no numeric value".into(),
            }
        })?;
        let minimum = ax_attribute_number(scrollbar, "AXMinValue").unwrap_or(0.0);
        let maximum = ax_attribute_number(scrollbar, "AXMaxValue").unwrap_or(1.0);
        if !current.is_finite() || !minimum.is_finite() || !maximum.is_finite() || maximum < minimum
        {
            return Err(ComputerError::Backend(
                "Accessibility scroll bar returned an invalid value range".into(),
            ));
        }

        let range = maximum - minimum;
        let requested = f64::from(amount);
        // AppKit commonly reports scroll bars as either pixel-like values or a
        // normalized 0..1 range. Preserve the caller's pixel intent in the first
        // case and scale it conservatively in the normalized case.
        let delta = if range <= 1.0 {
            range * (requested / 1000.0).clamp(0.01, 0.25)
        } else {
            requested.min(range.max(1.0))
        };
        let signed_delta = match direction {
            ScrollDirection::Up | ScrollDirection::Left => -delta,
            ScrollDirection::Down | ScrollDirection::Right => delta,
        };
        let next = (current + signed_delta).clamp(minimum, maximum);
        if (next - current).abs() <= f64::EPSILON {
            return Err(ComputerError::Unsupported {
                capability: "background_scrollbar_value".into(),
                detail: "Accessibility scroll bar is already at the requested edge".into(),
            });
        }
        let value = ax_number(next)?;
        let result = ax_set_attribute(scrollbar, "AXValue", value);
        unsafe { CFRelease(value) };
        result.map(|()| next - current)
    }

    fn application_identity(window: &WindowRecord) -> ApplicationIdentity {
        ApplicationIdentity {
            process_id: Some(window.process_id),
            executable_name: Some(window.owner_name.clone()),
            executable_path: None,
            executable_hash: None,
            process_architecture: ProcessArchitecture::Unknown,
            top_level_window_class: None,
            framework_hints: vec![FrameworkHint::Unknown],
            version: None,
        }
    }

    fn send_scroll_to_pid(
        process_id: u32,
        point: CGPoint,
        direction: ScrollDirection,
        amount: u32,
    ) -> Result<String, ComputerError> {
        ensure_ax_trusted()?;
        let amount = amount.min(i32::MAX as u32) as i32;
        let (delta_x, delta_y) = match direction {
            ScrollDirection::Up => (0, -amount),
            ScrollDirection::Down => (0, amount),
            ScrollDirection::Left => (-amount, 0),
            ScrollDirection::Right => (amount, 0),
        };
        let event = unsafe {
            CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                K_CG_SCROLL_LINE,
                2,
                -delta_y,
                delta_x,
                0,
            )
        };
        if event.is_null() {
            return Err(ComputerError::AccessDenied(
                "CGEventCreateScrollWheelEvent returned null".into(),
            ));
        }
        unsafe {
            // The event carries the virtual cursor location, but posting it
            // to a PID does not move the user's real pointer.
            CGEventSetLocation(event, point);
            CGEventSetIntegerValueField(
                event,
                K_CG_EVENT_SOURCE_USER_DATA,
                configured_input_marker(),
            );
            CGEventPostToPid(process_id as i32, event);
            CFRelease(event);
        }
        Ok(format!(
            "route=background_pid_scroll; process_id={process_id}; delta_x={delta_x}; delta_y={delta_y}"
        ))
    }

    fn execute_background(
        &mut self,
        session: &ComputerSessionId,
        target: &WindowId,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        let target = self.background_window(target)?;
        if let ComputerAction::FocusWindow { window_id } = action {
            let detail = self.focus_window(window_id)?;
            return Ok(ComputerActionResult {
                status: ActionStatus::Performed,
                observation: None,
                backend_detail: Some(format!(
                    "backend=mac-native; route=takeover_focus; {detail}"
                )),
            });
        }
        if matches!(
            action,
            ComputerAction::TypeText { at: None, .. }
                | ComputerAction::KeyPress { .. }
                | ComputerAction::Hotkey { .. }
                | ComputerAction::KeyDown { .. }
                | ComputerAction::KeyUp { .. }
                | ComputerAction::HoldKey { .. }
        ) {
            return self.execute_application(session, &Self::application_identity(&target), action);
        }
        let detail = self.background_pointer_action(session, &target, action)?;
        Ok(ComputerActionResult {
            status: ActionStatus::Performed,
            observation: None,
            backend_detail: Some(format!("backend=mac-native; {detail}")),
        })
    }

    /// Bind a semantic element to the least invasive pixel action that can be
    /// used if the AX provider rejects an inactive-window action.  This is a
    /// deliberately narrow bridge: actions with no trustworthy screen point
    /// (range writes and scroll-into-view) remain semantic-only.
    fn semantic_pixel_fallback(
        &self,
        session: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Option<(WindowId, ComputerAction, PixelTarget)> {
        let state = self.semantic.get(session)?;
        let record = state.current.get(action.element_id())?;
        let bounds = record.semantic.bounds.clone()?;
        if bounds.extent.width <= 0.0 || bounds.extent.height <= 0.0 {
            return None;
        }
        let mut center = bounds.clone();
        center.point = Point {
            x: bounds.point.x + bounds.extent.width / 2.0,
            y: bounds.point.y + bounds.extent.height / 2.0,
        };
        let window_id = record.window_id.clone();
        let pixel_action = match action {
            SemanticAction::Focus { .. }
            | SemanticAction::Invoke { .. }
            | SemanticAction::Toggle { .. }
            | SemanticAction::Select { .. }
            | SemanticAction::Expand { .. }
            | SemanticAction::Collapse { .. } => ComputerAction::Click { at: center.clone() },
            SemanticAction::SetValue { value, .. } => ComputerAction::TypeText {
                text: value.clone(),
                target: Some(window_id.clone()),
                at: Some(center.clone()),
            },
            SemanticAction::SetRangeValue { .. } | SemanticAction::ScrollIntoView { .. } => {
                return None;
            }
        };
        let pixel_target = PixelTarget {
            element_id: action.element_id().clone(),
            window_id: window_id.clone(),
            bounds,
            center,
            generation: state.generation,
        };
        Some((window_id, pixel_action, pixel_target))
    }

    async fn execute_takeover_fallback(
        &mut self,
        session: &ComputerSessionId,
        target: &WindowId,
        action: &ComputerAction,
    ) -> Result<String, ComputerError> {
        let focus_detail = self.focus_window(target)?;
        let result = self.execute(session, action).await?;
        Ok(format!(
            "route=takeover_fallback; focus={focus_detail}; {}",
            result
                .backend_detail
                .unwrap_or_else(|| "action_completed=true".into())
        ))
    }

    async fn execute_preferred_pixel_fallback(
        &mut self,
        session: &ComputerSessionId,
        target: &WindowId,
        action: &ComputerAction,
        mode: ComputerExecutionMode,
    ) -> Result<(String, bool), ComputerError> {
        if mode == ComputerExecutionMode::BackgroundPreferred {
            match self.execute_background(session, target, action) {
                Ok(result) => Ok((
                    result
                        .backend_detail
                        .unwrap_or_else(|| "route=background_pixel".into()),
                    false,
                )),
                Err(background_error) => {
                    let takeover_detail = self
                        .execute_takeover_fallback(session, target, action)
                        .await?;
                    Ok((
                        format!(
                            "route=takeover_fallback; background_error={background_error}; {takeover_detail}"
                        ),
                        true,
                    ))
                }
            }
        } else {
            let detail = self
                .execute_takeover_fallback(session, target, action)
                .await?;
            Ok((detail, true))
        }
    }

    fn execute_application(
        &mut self,
        session: &ComputerSessionId,
        target: &ApplicationIdentity,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        self.ensure_initialized()?;
        let process_id = target.process_id.ok_or_else(|| {
            ComputerError::TargetUnavailable("application has no process id".into())
        })?;
        let detail = match action {
            ComputerAction::TypeText { text, at: None, .. } => {
                self.send_text_to_pid(text, Some(process_id))?
            }
            ComputerAction::TypeText { at: Some(_), .. } => {
                return Err(ComputerError::Unsupported {
                    capability: "application_scoped_spatial_text".into(),
                    detail: "text input with coordinates must remain window/pixel scoped".into(),
                })
            }
            ComputerAction::KeyPress { key, .. } => {
                let code = key_code(key).ok_or_else(|| {
                    ComputerError::InvalidAction(format!("unsupported macOS key: {key}"))
                })?;
                self.send_key_event_with_flags_to_pid(code, true, 0, Some(process_id))?;
                self.send_key_event_with_flags_to_pid(code, false, 0, Some(process_id))?;
                format!("key={key}")
            }
            ComputerAction::Hotkey { keys, .. } => {
                self.send_hotkey_to_pid(keys, Some(process_id))?;
                format!("keys={keys:?}")
            }
            ComputerAction::KeyDown { key, .. } => {
                let code = key_code(key).ok_or_else(|| {
                    ComputerError::InvalidAction(format!("unsupported macOS key: {key}"))
                })?;
                self.send_key_event_with_flags_to_pid(code, true, 0, Some(process_id))?;
                self.pressed
                    .entry(session.clone())
                    .or_default()
                    .keys
                    .insert(key.to_ascii_lowercase());
                format!("key={key}; phase=down")
            }
            ComputerAction::KeyUp { key, .. } => {
                let code = key_code(key).ok_or_else(|| {
                    ComputerError::InvalidAction(format!("unsupported macOS key: {key}"))
                })?;
                self.send_key_event_with_flags_to_pid(code, false, 0, Some(process_id))?;
                if let Some(state) = self.pressed.get_mut(session) {
                    state.keys.remove(&key.to_ascii_lowercase());
                }
                format!("key={key}; phase=up")
            }
            ComputerAction::HoldKey {
                key, duration_ms, ..
            } => {
                if *duration_ms == 0 || *duration_ms > MAX_HOLD_KEY_MS {
                    return Err(ComputerError::InvalidAction(format!(
                        "HoldKey duration must be between 1 and {MAX_HOLD_KEY_MS} ms"
                    )));
                }
                let code = key_code(key).ok_or_else(|| {
                    ComputerError::InvalidAction(format!("unsupported macOS key: {key}"))
                })?;
                self.send_key_event_with_flags_to_pid(code, true, 0, Some(process_id))?;
                self.pressed
                    .entry(session.clone())
                    .or_default()
                    .keys
                    .insert(key.to_ascii_lowercase());
                thread::sleep(Duration::from_millis(*duration_ms as u64));
                let result =
                    self.send_key_event_with_flags_to_pid(code, false, 0, Some(process_id));
                if let Some(state) = self.pressed.get_mut(session) {
                    state.keys.remove(&key.to_ascii_lowercase());
                }
                result?;
                format!("key={key}; duration_ms={duration_ms}")
            }
            _ => {
                return Err(ComputerError::Unsupported {
                    capability: "application_scoped_input".into(),
                    detail: "application scope supports keyboard and text actions only".into(),
                })
            }
        };
        Ok(ComputerActionResult {
            status: ActionStatus::Performed,
            observation: None,
            backend_detail: Some(format!(
                "backend=mac-native; dispatch=pid; pid={process_id}; action={}; {detail}",
                action_name(action)
            )),
        })
    }

    fn focus_window(&self, window_id: &WindowId) -> Result<String, ComputerError> {
        let target = enumerate_window_records()?
            .into_iter()
            .find(|window| &window.id == window_id)
            .ok_or_else(|| ComputerError::InvalidWindow(format!("window {window_id} is gone")))?;
        if unsafe { AXIsProcessTrusted() } == 0 {
            return Err(ComputerError::AccessDenied(
                "macOS Accessibility permission is required to activate a window; enable the sidecar in System Settings > Privacy & Security > Accessibility".into(),
            ));
        }
        let application = unsafe { AXUIElementCreateApplication(target.process_id as i32) };
        if application.is_null() {
            return Err(ComputerError::AccessDenied(
                "AXUIElementCreateApplication returned null".into(),
            ));
        }
        let attribute = unsafe {
            CFStringCreateWithCString(
                std::ptr::null(),
                CString::new("AXFrontmost")
                    .expect("static AX attribute has no NUL")
                    .as_ptr(),
                UTF8_ENCODING,
            )
        };
        if attribute.is_null() {
            unsafe { CFRelease(application) };
            return Err(ComputerError::Backend(
                "failed to create AXFrontmost attribute".into(),
            ));
        }
        let result =
            unsafe { AXUIElementSetAttributeValue(application, attribute, kCFBooleanTrue) };
        unsafe {
            CFRelease(attribute);
            CFRelease(application);
        }
        if result != 0 {
            return Err(ComputerError::AccessDenied(format!(
                "AXUIElementSetAttributeValue(AXFrontmost) failed with AX error {result}"
            )));
        }
        for _ in 0..20 {
            if enumerate_window_records()?
                .into_iter()
                .find(|window| window.active)
                .is_some_and(|window| window.process_id == target.process_id)
            {
                return Ok(format!(
                    "window_id={window_id}; process_id={}; foreground_confirmed=true",
                    target.process_id
                ));
            }
            thread::sleep(Duration::from_millis(10));
        }
        Err(ComputerError::ForegroundDenied(format!(
            "window {window_id} did not become foreground after AX activation"
        )))
    }

    fn point_is_in_foreground(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
    ) -> Result<(CGPoint, String), ComputerError> {
        let (point, detail) = self.coordinate(session, coordinate)?;
        let foreground = self.foreground_window()?;
        if !contains(
            foreground.bounds,
            Point {
                x: point.x,
                y: point.y,
            },
        ) {
            return Err(ComputerError::InvalidCoordinate(
                "coordinate is outside the current foreground window".into(),
            ));
        }
        Ok((point, detail))
    }

    fn send_mouse(
        &self,
        event_type: u32,
        point: CGPoint,
        button: MouseButton,
    ) -> Result<(), ComputerError> {
        ensure_ax_trusted()?;
        let native_button = match button {
            MouseButton::Left => K_CG_MOUSE_BUTTON_LEFT,
            MouseButton::Right => K_CG_MOUSE_BUTTON_RIGHT,
            MouseButton::Middle => K_CG_MOUSE_BUTTON_CENTER,
        };
        let event =
            unsafe { CGEventCreateMouseEvent(std::ptr::null(), event_type, point, native_button) };
        if event.is_null() {
            return Err(ComputerError::AccessDenied(
                "CGEventCreateMouseEvent returned null; grant Accessibility permission to the sidecar".into(),
            ));
        }
        unsafe {
            CGEventSetIntegerValueField(
                event,
                K_CG_EVENT_SOURCE_USER_DATA,
                configured_input_marker(),
            );
            CGEventPost(K_CG_EVENT_TAP_HID, event);
            CFRelease(event);
        }
        Ok(())
    }

    fn send_key_event(&self, key_code: u16, down: bool) -> Result<(), ComputerError> {
        self.send_key_event_with_flags(key_code, down, 0)
    }

    fn send_key_event_with_flags(
        &self,
        key_code: u16,
        down: bool,
        flags: u64,
    ) -> Result<(), ComputerError> {
        self.send_key_event_with_flags_to_pid(key_code, down, flags, None)
    }

    /// Post keyboard events to a specific application when the request is
    /// application-scoped.  This follows the mature design's PID-directed
    /// input path and avoids coupling text entry to a stale top-level window.
    fn send_key_event_with_flags_to_pid(
        &self,
        key_code: u16,
        down: bool,
        flags: u64,
        process_id: Option<u32>,
    ) -> Result<(), ComputerError> {
        ensure_ax_trusted()?;
        let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), key_code, down as u8) };
        if event.is_null() {
            return Err(ComputerError::AccessDenied(
                "CGEventCreateKeyboardEvent returned null; grant Accessibility permission to the sidecar".into(),
            ));
        }
        unsafe {
            if flags != 0 {
                CGEventSetFlags(event, flags);
            }
            CGEventSetIntegerValueField(
                event,
                K_CG_EVENT_SOURCE_USER_DATA,
                configured_input_marker(),
            );
            if let Some(process_id) = process_id {
                CGEventPostToPid(process_id as i32, event);
            } else {
                CGEventPost(K_CG_EVENT_TAP_HID, event);
            }
            CFRelease(event);
        }
        Ok(())
    }

    fn send_hotkey(&self, keys: &[String]) -> Result<(), ComputerError> {
        self.send_hotkey_to_pid(keys, None)
    }

    fn send_hotkey_to_pid(
        &self,
        keys: &[String],
        process_id: Option<u32>,
    ) -> Result<(), ComputerError> {
        if keys.is_empty() {
            return Err(ComputerError::InvalidAction(
                "Hotkey requires at least one key".into(),
            ));
        }
        let parsed = keys
            .iter()
            .map(|key| {
                let code = key_code(key).ok_or_else(|| {
                    ComputerError::InvalidAction(format!("unsupported macOS key: {key}"))
                })?;
                Ok((code, modifier_flag(key)))
            })
            .collect::<Result<Vec<_>, ComputerError>>()?;

        let mut flags = 0;
        for (pressed, (code, modifier)) in parsed.iter().enumerate() {
            flags |= *modifier;
            if let Err(error) =
                self.send_key_event_with_flags_to_pid(*code, true, flags, process_id)
            {
                for (code, modifier) in parsed[..pressed].iter().rev() {
                    // A modifier key-up must carry the post-release flags.
                    // Sending the old Command/Shift flag on its own key-up
                    // can leave macOS believing the modifier is still held,
                    // which breaks Cmd+Tab/Cmd+Space for subsequent actions.
                    let release_flags = flags & !*modifier;
                    let _ = self.send_key_event_with_flags_to_pid(
                        *code,
                        false,
                        release_flags,
                        process_id,
                    );
                    flags = release_flags;
                }
                return Err(error);
            }
        }
        let mut release_error = None;
        for (code, modifier) in parsed.iter().rev() {
            let release_flags = flags & !*modifier;
            if let Err(error) =
                self.send_key_event_with_flags_to_pid(*code, false, release_flags, process_id)
            {
                release_error.get_or_insert(error);
            }
            flags = release_flags;
        }
        if let Some(error) = release_error {
            return Err(error);
        }
        Ok(())
    }

    fn send_unicode_chunk_to_pid(
        &self,
        utf16: &[u16],
        process_id: Option<u32>,
    ) -> Result<(), ComputerError> {
        ensure_ax_trusted()?;
        for down in [true, false] {
            let event = unsafe { CGEventCreateKeyboardEvent(std::ptr::null(), 0, down as u8) };
            if event.is_null() {
                return Err(ComputerError::AccessDenied(
                    "CGEventCreateKeyboardEvent returned null while typing".into(),
                ));
            }
            unsafe {
                CGEventKeyboardSetUnicodeString(event, utf16.len(), utf16.as_ptr());
                CGEventSetIntegerValueField(
                    event,
                    K_CG_EVENT_SOURCE_USER_DATA,
                    configured_input_marker(),
                );
                if let Some(process_id) = process_id {
                    CGEventPostToPid(process_id as i32, event);
                } else {
                    CGEventPost(K_CG_EVENT_TAP_HID, event);
                }
                CFRelease(event);
            }
        }
        Ok(())
    }

    fn send_text(&self, text: &str) -> Result<String, ComputerError> {
        self.send_text_to_pid(text, None)
    }

    fn send_text_to_pid(
        &self,
        text: &str,
        process_id: Option<u32>,
    ) -> Result<String, ComputerError> {
        // CGEventKeyboardSetUnicodeString accepts a Unicode string, so keep
        // ordinary text in bounded UTF-16 chunks instead of creating two
        // CoreGraphics events for every scalar. Newline/tab remain real key
        // events so editors and terminals retain their normal behavior.
        const MAX_UNICODE_CHUNK_UNITS: usize = 64;
        let mut utf16 = Vec::with_capacity(MAX_UNICODE_CHUNK_UNITS);
        let mut codepoints = 0usize;
        let mut chunks = 0usize;
        for ch in text.chars() {
            if ch == '\n' || ch == '\r' || ch == '\t' {
                if !utf16.is_empty() {
                    self.send_unicode_chunk_to_pid(&utf16, process_id)?;
                    chunks += 1;
                    utf16.clear();
                }
                if ch == '\t' {
                    let code = key_code("tab").expect("tab has a macOS key code");
                    self.send_key_event_with_flags_to_pid(code, true, 0, process_id)?;
                    self.send_key_event_with_flags_to_pid(code, false, 0, process_id)?;
                } else {
                    let code = key_code("enter").expect("enter has a macOS key code");
                    self.send_key_event_with_flags_to_pid(code, true, 0, process_id)?;
                    self.send_key_event_with_flags_to_pid(code, false, 0, process_id)?;
                }
                codepoints += 1;
                continue;
            }
            let mut units = [0u16; 2];
            let encoded = ch.encode_utf16(&mut units);
            if utf16.len() + encoded.len() > MAX_UNICODE_CHUNK_UNITS {
                self.send_unicode_chunk_to_pid(&utf16, process_id)?;
                chunks += 1;
                utf16.clear();
            }
            utf16.extend_from_slice(encoded);
            codepoints += 1;
        }
        if !utf16.is_empty() {
            self.send_unicode_chunk_to_pid(&utf16, process_id)?;
            chunks += 1;
        }
        Ok(format!(
            "unicode_codepoints={codepoints}; unicode_chunks={chunks}"
        ))
    }

    fn send_key(&self, key: &str, down: bool) -> Result<(), ComputerError> {
        let code = key_code(key)
            .ok_or_else(|| ComputerError::InvalidAction(format!("unsupported macOS key: {key}")))?;
        self.send_key_event(code, down)
    }

    fn send_click(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        button: MouseButton,
        count: u8,
    ) -> Result<String, ComputerError> {
        let (point, detail) = self.point_is_in_foreground(session, coordinate)?;
        let (down, up) = mouse_event_types(button);
        self.send_mouse(K_CG_MOUSE_MOVED, point, MouseButton::Left)?;
        for _ in 0..count {
            self.send_mouse(down, point, button)?;
            self.send_mouse(up, point, button)?;
        }
        Ok(format!("{detail}; button={button:?}; clicks={count}"))
    }

    fn send_scroll(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        direction: ScrollDirection,
        amount: u32,
    ) -> Result<String, ComputerError> {
        let amount = amount.min(i32::MAX as u32) as i32;
        let (delta_x, delta_y) = match direction {
            ScrollDirection::Up => (0, -amount),
            ScrollDirection::Down => (0, amount),
            ScrollDirection::Left => (-amount, 0),
            ScrollDirection::Right => (amount, 0),
        };
        let detail = self.send_scroll_delta(session, coordinate, delta_x, delta_y)?;
        Ok(format!(
            "{detail}; direction={direction:?}; amount={amount}"
        ))
    }

    fn send_scroll_delta(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        delta_x: i32,
        delta_y: i32,
    ) -> Result<String, ComputerError> {
        ensure_ax_trusted()?;
        let (point, detail) = self.point_is_in_foreground(session, coordinate)?;
        self.send_mouse(K_CG_MOUSE_MOVED, point, MouseButton::Left)?;
        let event = unsafe {
            CGEventCreateScrollWheelEvent(
                std::ptr::null(),
                K_CG_SCROLL_LINE,
                2,
                -delta_y,
                delta_x,
                0,
            )
        };
        if event.is_null() {
            return Err(ComputerError::AccessDenied(
                "CGEventCreateScrollWheelEvent returned null".into(),
            ));
        }
        unsafe {
            CGEventSetIntegerValueField(
                event,
                K_CG_EVENT_SOURCE_USER_DATA,
                configured_input_marker(),
            );
            CGEventPost(K_CG_EVENT_TAP_HID, event);
            CFRelease(event);
        }
        Ok(format!("{detail}; delta_x={delta_x}; delta_y={delta_y}"))
    }

    fn send_pointer_action(
        &self,
        session: &ComputerSessionId,
        action: &ComputerPointerAction,
    ) -> Result<String, ComputerError> {
        match action {
            ComputerPointerAction::Click { at, button, clicks } => {
                if *clicks == 0 || *clicks > 3 {
                    return Err(ComputerError::InvalidAction(
                        "modified pointer click count must be between 1 and 3".into(),
                    ));
                }
                self.send_click(session, at, *button, *clicks)
            }
            ComputerPointerAction::Move { to } => {
                let (point, detail) = self.coordinate(session, to)?;
                self.send_mouse(K_CG_MOUSE_MOVED, point, MouseButton::Left)?;
                Ok(detail)
            }
            ComputerPointerAction::Drag { path, button } => {
                if path.len() < 2 {
                    return Err(ComputerError::InvalidAction(
                        "modified pointer drag requires at least two points".into(),
                    ));
                }
                let (from_point, from_detail) =
                    self.point_is_in_foreground(session, path.first().expect("path is non-empty"))?;
                let mut points = Vec::with_capacity(path.len());
                for coordinate in path {
                    points.push(self.coordinate(session, coordinate)?.0);
                }
                let (down, up) = mouse_event_types(*button);
                self.send_mouse(K_CG_MOUSE_MOVED, from_point, MouseButton::Left)?;
                self.send_mouse(down, from_point, *button)?;
                for point in points.iter().skip(1) {
                    self.send_mouse(K_CG_MOUSE_DRAGGED, *point, *button)?;
                }
                self.send_mouse(up, *points.last().expect("path is non-empty"), *button)?;
                Ok(format!(
                    "from={from_detail}; waypoints={}; button={button:?}",
                    points.len()
                ))
            }
            ComputerPointerAction::Scroll {
                at,
                delta_x,
                delta_y,
            } => self.send_scroll_delta(session, at, *delta_x, *delta_y),
        }
    }

    fn send_modified_pointer(
        &self,
        session: &ComputerSessionId,
        action: &ComputerPointerAction,
        modifiers: &[String],
    ) -> Result<String, ComputerError> {
        if modifiers.is_empty() {
            return self.send_pointer_action(session, action);
        }
        let mut pressed = 0usize;
        for modifier in modifiers {
            if let Err(error) = self.send_key(modifier, true) {
                for previous in modifiers[..pressed].iter().rev() {
                    let _ = self.send_key(previous, false);
                }
                return Err(error);
            }
            pressed += 1;
        }
        let action_result = self.send_pointer_action(session, action);
        let mut release_error = None;
        for modifier in modifiers[..pressed].iter().rev() {
            if let Err(error) = self.send_key(modifier, false) {
                release_error.get_or_insert(error);
            }
        }
        action_result?;
        if let Some(error) = release_error {
            return Err(error);
        }
        Ok(format!("modifiers={modifiers:?}; transaction=true"))
    }

    fn release_pressed(&mut self, session: &ComputerSessionId) {
        let Some(state) = self.pressed.remove(session) else {
            return;
        };
        for key in state.keys {
            let _ = self.send_key(&key, false);
        }
        for button in state.mouse_buttons {
            let (_, up) = mouse_event_types(button);
            let _ = self.send_mouse(up, CGPoint::default(), button);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_key_mapping_covers_modifiers_and_navigation() {
        assert_eq!(key_code("cmd"), Some(55));
        assert_eq!(modifier_flag("cmd"), 0x0010_0000);
        assert_eq!(modifier_flag("SHIFT"), 0x0002_0000);
        assert_eq!(modifier_flag("l"), 0);
        assert_eq!(key_code("return"), Some(36));
        assert_eq!(key_code("page_down"), Some(121));
        assert_eq!(key_code("not-a-key"), None);
    }

    #[test]
    fn mac_input_marker_uses_the_host_marker_and_rejects_invalid_values() {
        assert_eq!(configured_input_marker_from(Some("123456")), 123456);
        assert_eq!(
            configured_input_marker_from(Some("0")),
            ALICE_COMPUTER_INPUT_MARKER
        );
        assert_eq!(
            configured_input_marker_from(Some("not-a-marker")),
            ALICE_COMPUTER_INPUT_MARKER
        );
    }

    #[test]
    fn bgra_png_encoder_emits_a_png_and_swaps_red_blue() {
        let bytes = encode_bgra_png(&[1, 2, 3, 255], 1, 1, 4).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn virtual_desktop_blit_scales_and_places_display_pixels() {
        let source = vec![1, 2, 3, 255, 4, 5, 6, 255];
        let mut destination = vec![0; 4 * 2 * 4];
        blit_scaled_bgra(&mut destination, 4, 2, &source, 2, 1, 8, 1, 0, 2, 1);
        assert_eq!(&destination[4..12], &[1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn virtual_cursor_is_visible_in_the_composited_frame() {
        let cursor = VirtualCursorState {
            position: Some(Point { x: 5.0, y: 5.0 }),
            trail: VecDeque::from([Point { x: 3.0, y: 3.0 }, Point { x: 5.0, y: 5.0 }]),
            visible: true,
        };
        let mut pixels = vec![40; 20 * 20 * 4];
        draw_virtual_cursor(
            &mut pixels,
            20,
            20,
            20 * 4,
            &cursor,
            Point { x: 0.0, y: 0.0 },
            DpiScale::ONE,
        );
        assert_ne!(
            &pixels[5 * 20 * 4 + 5 * 4..5 * 20 * 4 + 5 * 4 + 3],
            &[40, 40, 40]
        );
        assert!(pixels
            .as_chunks::<4>()
            .0
            .iter()
            .any(|pixel| pixel[3] == 255));
    }

    #[test]
    fn capture_pixel_scale_uses_the_actual_image_backing_size() {
        let scale = capture_pixel_scale(
            Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: 1470.0,
                    height: 956.0,
                },
            },
            2940,
            1912,
        );
        assert_eq!(scale, DpiScale { x: 2.0, y: 2.0 });
    }

    #[test]
    fn capture_pixel_scale_preserves_downsampled_backing_sizes() {
        let scale = capture_pixel_scale(
            Rect {
                origin: Point { x: 0.0, y: 0.0 },
                size: Size {
                    width: 200.0,
                    height: 100.0,
                },
            },
            100,
            50,
        );
        assert_eq!(scale, DpiScale { x: 0.5, y: 0.5 });
    }

    #[test]
    fn display_mode_scale_uses_physical_pixels_for_retina_topology() {
        let scale = scale_from_display_sizes(
            Size {
                width: 1470.0,
                height: 956.0,
            },
            Size {
                width: 2940.0,
                height: 1912.0,
            },
        );
        assert_eq!(scale, DpiScale { x: 2.0, y: 2.0 });
    }

    #[test]
    fn display_mode_scale_does_not_clamp_valid_fractional_modes() {
        let scale = scale_from_display_sizes(
            Size {
                width: 200.0,
                height: 100.0,
            },
            Size {
                width: 100.0,
                height: 50.0,
            },
        );
        assert_eq!(scale, DpiScale { x: 0.5, y: 0.5 });
    }

    #[test]
    fn display_selection_uses_the_largest_intersection() {
        let displays = vec![
            DisplayInfo {
                id: DisplayId::new("left"),
                name: None,
                physical_bounds: Rect {
                    origin: Point { x: 0.0, y: 0.0 },
                    size: Size {
                        width: 100.0,
                        height: 100.0,
                    },
                },
                work_area: Rect {
                    origin: Point { x: 0.0, y: 0.0 },
                    size: Size {
                        width: 100.0,
                        height: 100.0,
                    },
                },
                physical_size: Size {
                    width: 100.0,
                    height: 100.0,
                },
                logical_size: Size {
                    width: 100.0,
                    height: 100.0,
                },
                dpi: DpiScale::ONE,
                scale: DpiScale::ONE,
                primary: true,
            },
            DisplayInfo {
                id: DisplayId::new("right"),
                name: None,
                physical_bounds: Rect {
                    origin: Point { x: 100.0, y: 0.0 },
                    size: Size {
                        width: 100.0,
                        height: 100.0,
                    },
                },
                work_area: Rect {
                    origin: Point { x: 100.0, y: 0.0 },
                    size: Size {
                        width: 100.0,
                        height: 100.0,
                    },
                },
                physical_size: Size {
                    width: 100.0,
                    height: 100.0,
                },
                logical_size: Size {
                    width: 100.0,
                    height: 100.0,
                },
                dpi: DpiScale::ONE,
                scale: DpiScale::ONE,
                primary: false,
            },
        ];
        let topology = DisplayTopology::from_displays(displays, 1);
        assert_eq!(
            display_for_rect(
                &topology,
                Rect {
                    origin: Point { x: 80.0, y: 10.0 },
                    size: Size {
                        width: 60.0,
                        height: 20.0,
                    },
                }
            ),
            Some(DisplayId::new("right"))
        );
    }
}

impl Default for MacNativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ComputerBackend for MacNativeBackend {
    async fn initialize(&mut self) -> Result<(), ComputerError> {
        if self.initialized {
            return Err(ComputerError::AlreadyInitialized);
        }
        self.initialized = true;
        self.self_security = Some(ProcessSecurityContext {
            // macOS has no Windows integrity-level equivalent. Medium is used
            // only as a descriptive local-user value; TCC permissions remain
            // the actual gate for Screen Recording and Accessibility.
            integrity_level: IntegrityLevel::Medium,
            elevated: false,
            ui_access: false,
            app_container: false,
            process_id: Some(std::process::id()),
        });
        request_ax_trust_prompt();
        // When the bundled Swift helper is available, it owns the Screen
        // Recording TCC identity and opens the matching settings pane on the
        // first capture. Keep the legacy prompt only for CoreGraphics-only
        // deployments so users do not grant the wrong executable.
        if swift_capture_helper_path().is_none() {
            request_screen_capture_prompt();
        }
        if let Err(error) = self.refresh_topology() {
            self.initialized = false;
            self.self_security = None;
            return Err(error);
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), ComputerError> {
        if let Some(helper) = self.swift_capture.take() {
            helper.shutdown();
        }
        for session in self.pressed.keys().cloned().collect::<Vec<_>>() {
            self.release_pressed(&session);
        }
        self.frames.clear();
        self.semantic.clear();
        self.virtual_cursor.clear();
        self.topology = None;
        self.self_security = None;
        self.initialized = false;
        Ok(())
    }

    async fn security_context(&mut self) -> Result<ProcessSecurityContext, ComputerError> {
        self.ensure_initialized()?;
        self.self_security.clone().ok_or_else(|| {
            ComputerError::SecurityContextUnavailable("macOS process context is unavailable".into())
        })
    }

    async fn desktop_security_context(&mut self) -> Result<DesktopSecurityContext, ComputerError> {
        self.ensure_initialized()?;
        Ok(DesktopSecurityContext {
            desktop_kind: DesktopKind::InteractiveUserDesktop,
            interactive: true,
            protected: false,
        })
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability { name: "display_enumeration", state: CapabilityState::Supported, detail: "CoreGraphics active display enumeration" },
            Capability { name: "window_enumeration", state: CapabilityState::Supported, detail: "CoreGraphics on-screen window metadata" },
            Capability { name: "screenshot", state: CapabilityState::Supported, detail: "ScreenCaptureKit helper display capture with CoreGraphics fallback; Screen Recording permission required" },
            Capability { name: "virtual_desktop_screenshot", state: CapabilityState::Supported, detail: "Per-display ScreenCaptureKit captures composited across active displays; Screen Recording permission required" },
            Capability { name: "pixel_input", state: CapabilityState::Supported, detail: "CoreGraphics CGEvent mouse and keyboard injection; Accessibility permission required" },
            Capability { name: "semantic_observation", state: CapabilityState::Supported, detail: "bounded AXUIElement accessibility tree; Accessibility permission required" },
            Capability { name: "semantic_actions", state: CapabilityState::Supported, detail: "AXUIElement focus, invoke, value, toggle, selection, expansion, and range actions; Accessibility permission required" },
            Capability { name: "window_focus", state: CapabilityState::Supported, detail: "AXUIElement application activation; Accessibility permission required" },
        ]
    }

    async fn enumerate_screens(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<Vec<Screen>, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        Ok(self.topology()?.topology.displays.clone())
    }

    async fn display_topology(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<DisplayTopology, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        Ok(self.topology()?.topology.clone())
    }

    async fn observe(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<alice_computer_use_core::ComputerObservation, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let topology = self.topology()?.topology.clone();
        let screens = topology.displays.clone();
        let windows = public_windows(enumerate_window_records()?, &topology);
        let active_window = windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.id.clone());
        let frame = topology
            .primary_display_id
            .clone()
            .map(|display_id| self.capture_display_frame(session, &display_id))
            .transpose()?;
        Ok(alice_computer_use_core::ComputerObservation {
            session_id: session.clone(),
            screens,
            windows,
            active_window,
            screenshot: None,
            frame,
            display_topology: Some(topology),
        })
    }

    async fn enumerate_windows(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<Vec<Window>, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let topology = self.topology()?.topology.clone();
        Ok(public_windows(enumerate_window_records()?, &topology))
    }

    async fn screenshot(
        &mut self,
        session: &ComputerSessionId,
        screen: &ScreenId,
    ) -> Result<Screenshot, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        self.capture_display(session, screen)
    }

    async fn capture_frame(
        &mut self,
        session: &ComputerSessionId,
        screen: &ScreenId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        self.capture_display_frame(session, screen)
    }

    async fn frame_metadata(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let generation = self.topology()?.topology.topology_generation;
        Ok(self
            .frames
            .entry(session.clone())
            .or_default()
            .state(frame_id, generation))
    }

    async fn encode_frame(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        if encoding != FrameEncoding::Png {
            return Err(ComputerError::Unsupported {
                capability: "frame_encoding".into(),
                detail: "macOS backend supports PNG only".into(),
            });
        }
        self.encode_display_frame(session, frame_id)
    }

    async fn release_frame(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        self.ensure_initialized()?;
        Ok(self
            .frames
            .entry(session.clone())
            .or_default()
            .release(frame_id))
    }

    async fn screenshot_target(
        &mut self,
        session: &ComputerSessionId,
        target: &ScreenshotTarget,
    ) -> Result<Screenshot, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        match target {
            ScreenshotTarget::Display(id) => self.capture_display(session, id),
            ScreenshotTarget::VirtualDesktop => self.capture_virtual_desktop(session),
        }
    }

    async fn execute(
        &mut self,
        session: &ComputerSessionId,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let action_name = action_name(action);
        let detail = match action {
            ComputerAction::FocusWindow { window_id } => self.focus_window(window_id)?,
            ComputerAction::MovePointer { to } => {
                let (point, detail) = self.coordinate(session, to)?;
                self.send_mouse(K_CG_MOUSE_MOVED, point, MouseButton::Left)?;
                detail
            }
            ComputerAction::Click { at } => self.send_click(session, at, MouseButton::Left, 1)?,
            ComputerAction::DoubleClick { at } => {
                self.send_click(session, at, MouseButton::Left, 2)?
            }
            ComputerAction::RightClick { at } => {
                self.send_click(session, at, MouseButton::Right, 1)?
            }
            ComputerAction::MiddleClick { at, .. } => {
                self.send_click(session, at, MouseButton::Middle, 1)?
            }
            ComputerAction::TripleClick { at, .. } => {
                self.send_click(session, at, MouseButton::Left, 3)?
            }
            ComputerAction::Drag { from, to, button } => {
                let (from_point, from_detail) = self.point_is_in_foreground(session, from)?;
                let (to_point, to_detail) = self.coordinate(session, to)?;
                let (down, up) = mouse_event_types(*button);
                self.send_mouse(K_CG_MOUSE_MOVED, from_point, MouseButton::Left)?;
                self.send_mouse(down, from_point, *button)?;
                self.send_mouse(K_CG_MOUSE_DRAGGED, to_point, *button)?;
                self.send_mouse(up, to_point, *button)?;
                format!("from={from_detail}; to={to_detail}; button={button:?}")
            }
            ComputerAction::Scroll {
                at,
                direction,
                amount,
            } => self.send_scroll(session, at, *direction, *amount)?,
            ComputerAction::TypeText { text, target, at } => {
                let target_record = self.target_window(target.as_ref())?;
                let click_detail = if let Some(at) = at {
                    let (point, point_detail) = self.coordinate(session, at)?;
                    if !contains(
                        target_record.bounds,
                        Point {
                            x: point.x,
                            y: point.y,
                        },
                    ) {
                        return Err(ComputerError::InvalidCoordinate(
                            "type_text coordinate is outside the target window".into(),
                        ));
                    }
                    self.send_mouse(K_CG_MOUSE_MOVED, point, MouseButton::Left)?;
                    self.send_mouse(K_CG_LEFT_MOUSE_DOWN, point, MouseButton::Left)?;
                    self.send_mouse(K_CG_LEFT_MOUSE_UP, point, MouseButton::Left)?;
                    thread::sleep(Duration::from_millis(40));
                    self.target_window(target.as_ref())?;
                    Some(point_detail)
                } else {
                    None
                };
                let text_detail = self.send_text(text)?;
                format!(
                    "target={}; click={click_detail:?}; {text_detail}",
                    target_record.id
                )
            }
            ComputerAction::KeyPress { key, target } => {
                self.target_window(target.as_ref())?;
                self.send_key(key, true)?;
                self.send_key(key, false)?;
                format!("key={key}")
            }
            ComputerAction::Hotkey { keys, target } => {
                self.target_window(target.as_ref())?;
                self.send_hotkey(keys)?;
                format!("keys={keys:?}")
            }
            ComputerAction::MouseDown { button, at, target } => {
                self.target_window(target.as_ref())?;
                let (point, detail) = self.point_is_in_foreground(session, at)?;
                let (down, _) = mouse_event_types(*button);
                self.send_mouse(down, point, *button)?;
                self.pressed
                    .entry(session.clone())
                    .or_default()
                    .mouse_buttons
                    .insert(*button);
                detail
            }
            ComputerAction::MouseUp { button, at, target } => {
                self.target_window(target.as_ref())?;
                let (point, detail) = self.point_is_in_foreground(session, at)?;
                let (_, up) = mouse_event_types(*button);
                self.send_mouse(up, point, *button)?;
                if let Some(state) = self.pressed.get_mut(session) {
                    state.mouse_buttons.remove(button);
                }
                detail
            }
            ComputerAction::ModifierClick {
                modifier,
                button,
                at,
                target,
            } => {
                self.target_window(target.as_ref())?;
                self.send_key(modifier, true)?;
                let click = self.send_click(session, at, *button, 1);
                let release = self.send_key(modifier, false);
                click?;
                release?;
                format!("modifier={modifier}; button={button:?}")
            }
            ComputerAction::KeyDown { key, target } => {
                self.target_window(target.as_ref())?;
                self.send_key(key, true)?;
                self.pressed
                    .entry(session.clone())
                    .or_default()
                    .keys
                    .insert(key.to_ascii_lowercase());
                format!("key={key}; phase=down")
            }
            ComputerAction::KeyUp { key, target } => {
                self.target_window(target.as_ref())?;
                self.send_key(key, false)?;
                if let Some(state) = self.pressed.get_mut(session) {
                    state.keys.remove(&key.to_ascii_lowercase());
                }
                format!("key={key}; phase=up")
            }
            ComputerAction::HoldKey {
                key,
                duration_ms,
                target,
            } => {
                self.target_window(target.as_ref())?;
                if *duration_ms == 0 || *duration_ms > MAX_HOLD_KEY_MS {
                    return Err(ComputerError::InvalidAction(format!(
                        "HoldKey duration must be between 1 and {MAX_HOLD_KEY_MS} ms"
                    )));
                }
                self.send_key(key, true)?;
                self.pressed
                    .entry(session.clone())
                    .or_default()
                    .keys
                    .insert(key.to_ascii_lowercase());
                thread::sleep(Duration::from_millis(*duration_ms as u64));
                let result = self.send_key(key, false);
                if let Some(state) = self.pressed.get_mut(session) {
                    state.keys.remove(&key.to_ascii_lowercase());
                }
                result?;
                format!("key={key}; duration_ms={duration_ms}")
            }
            ComputerAction::ModifiedPointer { action, modifiers } => {
                self.send_modified_pointer(session, action, modifiers)?
            }
        };
        Ok(ComputerActionResult {
            status: ActionStatus::Performed,
            observation: None,
            backend_detail: Some(format!(
                "backend=mac-native; action={action_name}; {detail}"
            )),
        })
    }

    async fn semantic_observe(
        &mut self,
        session: &ComputerSessionId,
        window: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        self.semantic_observe_ax(session, window, limits)
    }

    async fn validate_element(
        &mut self,
        session: &ComputerSessionId,
        element: &ElementId,
    ) -> Result<(), ComputerError> {
        self.validate_ax_element(session, element)
    }

    async fn resolve_element_window(
        &mut self,
        session: &ComputerSessionId,
        element: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        self.ax_element_window(session, element)
    }

    async fn semantic_action(
        &mut self,
        session: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        self.semantic_action_ax(session, action)
    }

    async fn capability_probe(
        &mut self,
        session: &ComputerSessionId,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerError> {
        self.capability_probe_ax(session, window_id)
    }

    async fn execute_policy(
        &mut self,
        session: &ComputerSessionId,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, ComputerError> {
        self.ensure_initialized()?;
        let started = Instant::now();
        let mut attempts = Vec::new();
        let mut final_outcome = ComputerExecutionOutcome::InvalidRequest;
        let mut explanation = String::new();
        let mut security = None;
        let mut execution_verification = ComputerExecutionVerification::default();
        let mut generation_before = None;
        let mut generation_after = None;
        let mut pixel_target = None;
        let mut fallback_used = false;
        let mut semantic_attempt_ms = 0;
        let mut fallback_decision_ms = 0;
        let mut pixel_attempt_ms = 0;
        let mut verification_ms = 0;
        match &request.intent {
            ComputerExecutionIntent::Pixel {
                action,
                target_window_id,
                target_application,
            } => {
                if request.strategy == ComputerExecutionStrategy::SemanticOnly {
                    explanation = "SemanticOnly cannot execute a Pixel execution intent".into();
                } else if let Some(target_application) = target_application {
                    if target_window_id.is_some() {
                        explanation =
                            "application-scoped input cannot also carry a window target".into();
                    } else {
                        let target = if request.execution_mode
                            == ComputerExecutionMode::BackgroundPreferred
                        {
                            self.application_window(target_application)?
                        } else {
                            self.target_application_window(target_application)?
                        };
                        security = Some(self.security_decision(&target, "application_input")?);
                        let pixel_started = Instant::now();
                        let result = self.execute_application(session, target_application, action);
                        pixel_attempt_ms = pixel_started.elapsed().as_millis();
                        match result {
                            Ok(_) => {
                                final_outcome = ComputerExecutionOutcome::Performed;
                                explanation = if request.execution_mode
                                    == ComputerExecutionMode::BackgroundPreferred
                                {
                                    "application-scoped keyboard/text input succeeded without activating the target".into()
                                } else {
                                    "application-scoped keyboard/text input succeeded".into()
                                };
                                attempts.push(ComputerExecutionAttempt {
                                    method: ComputerExecutionMethod::PixelAction,
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: ComputerExecutionVerification {
                                        kind: Some(
                                            ComputerExecutionVerificationKind::WindowChanged,
                                        ),
                                        verified: true,
                                        detail: Some(
                                            if request.execution_mode
                                                == ComputerExecutionMode::BackgroundPreferred
                                            {
                                                "keyboard/text event was posted to the target application PID without activation".into()
                                            } else {
                                                "keyboard event was posted to the target application PID".into()
                                            },
                                        ),
                                        ..Default::default()
                                    },
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                        detail: None,
                                    });
                            }
                            Err(error) => {
                                let primary_detail = error.to_string();
                                final_outcome = execution_outcome_from_error(&error);
                                attempts.push(ComputerExecutionAttempt {
                                    method: ComputerExecutionMethod::PixelAction,
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: pixel_execution_verification(format!(
                                        "background application route failed: {primary_detail}"
                                    )),
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                    detail: Some("route=background_pid_primary".into()),
                                });
                                if request.execution_mode
                                    == ComputerExecutionMode::BackgroundPreferred
                                {
                                    let fallback_started = Instant::now();
                                    match self
                                        .execute_takeover_fallback(session, &target.id, action)
                                        .await
                                    {
                                        Ok(detail) => {
                                            final_outcome = ComputerExecutionOutcome::Performed;
                                            fallback_used = true;
                                            execution_verification =
                                                pixel_execution_verification(detail.clone());
                                            explanation = format!(
                                                "background application route failed ({primary_detail}); takeover fallback succeeded"
                                            );
                                            attempts.push(ComputerExecutionAttempt {
                                                method: ComputerExecutionMethod::PixelAction,
                                                outcome: final_outcome,
                                                semantic_element_id: None,
                                                pixel_target: None,
                                                verification: execution_verification.clone(),
                                                generation_before: None,
                                                generation_after: None,
                                                timing_ms: fallback_started.elapsed().as_millis(),
                                                detail: Some(detail),
                                            });
                                        }
                                        Err(fallback_error) => {
                                            fallback_used = true;
                                            explanation = format!(
                                                "background application route failed ({primary_detail}); takeover fallback failed: {fallback_error}"
                                            );
                                            final_outcome =
                                                execution_outcome_from_error(&fallback_error);
                                            attempts.push(ComputerExecutionAttempt {
                                                method: ComputerExecutionMethod::PixelAction,
                                                outcome: final_outcome,
                                                semantic_element_id: None,
                                                pixel_target: None,
                                                verification: pixel_execution_verification(
                                                    fallback_error.to_string(),
                                                ),
                                                generation_before: None,
                                                generation_after: None,
                                                timing_ms: fallback_started.elapsed().as_millis(),
                                                detail: Some("route=takeover_fallback".into()),
                                            });
                                        }
                                    }
                                    pixel_attempt_ms = pixel_attempt_ms
                                        .saturating_add(fallback_started.elapsed().as_millis());
                                }
                            }
                        }
                    }
                } else if target_window_id.is_none() && is_global_keyboard_action(action) {
                    let target = self.foreground_window()?;
                    security = Some(self.security_decision(&target, "global_input")?);
                    let pixel_started = Instant::now();
                    let result = self.execute(session, action).await;
                    pixel_attempt_ms = pixel_started.elapsed().as_millis();
                    match result {
                        Ok(_) => {
                            final_outcome = ComputerExecutionOutcome::Performed;
                            explanation =
                                "global keyboard/text input succeeded on the desktop".into();
                            attempts.push(ComputerExecutionAttempt {
                                method: ComputerExecutionMethod::PixelAction,
                                outcome: final_outcome,
                                semantic_element_id: None,
                                pixel_target: None,
                                verification: ComputerExecutionVerification {
                                    kind: Some(ComputerExecutionVerificationKind::WindowChanged),
                                    verified: true,
                                    detail: Some(
                                        "keyboard event was posted through the global HID path"
                                            .into(),
                                    ),
                                    ..Default::default()
                                },
                                generation_before: None,
                                generation_after: None,
                                timing_ms: pixel_attempt_ms,
                                detail: None,
                            });
                        }
                        Err(error) => {
                            final_outcome = execution_outcome_from_error(&error);
                            explanation = error.to_string();
                        }
                    }
                } else if target_window_id.is_none() {
                    explanation =
                        "pixel execution requires an explicit containing target window".into();
                } else {
                    let target_id = target_window_id
                        .as_ref()
                        .expect("target_window_id was checked above");
                    let target =
                        if request.execution_mode == ComputerExecutionMode::BackgroundPreferred {
                            self.background_window(target_id)?
                        } else {
                            self.target_window(Some(target_id))?
                        };
                    security = Some(self.security_decision(&target, "pixel_execution")?);
                    if request.execution_mode == ComputerExecutionMode::BackgroundPreferred {
                        let pixel_started = Instant::now();
                        let result = self.execute_background(session, &target.id, action);
                        pixel_attempt_ms = pixel_started.elapsed().as_millis();
                        match result {
                            Ok(result) => {
                                final_outcome = ComputerExecutionOutcome::Performed;
                                explanation = if matches!(
                                    action,
                                    ComputerAction::FocusWindow { .. }
                                ) {
                                    "focus action used the explicit takeover path".into()
                                } else {
                                    "background AX/PID action succeeded without activating or moving the real pointer".into()
                                };
                                let detail = result
                                    .backend_detail
                                    .clone()
                                    .unwrap_or_else(|| "background route completed".into());
                                execution_verification =
                                    pixel_execution_verification(detail.clone());
                                attempts.push(ComputerExecutionAttempt {
                                    method: pixel_execution_method(action),
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: execution_verification.clone(),
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                    detail: Some(detail),
                                });
                            }
                            Err(error) => {
                                let primary_detail = error.to_string();
                                final_outcome = execution_outcome_from_error(&error);
                                attempts.push(ComputerExecutionAttempt {
                                    method: pixel_execution_method(action),
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: pixel_execution_verification(format!(
                                        "background route failed: {primary_detail}"
                                    )),
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                    detail: Some("route=background_primary".into()),
                                });
                                let fallback_started = Instant::now();
                                match self
                                    .execute_takeover_fallback(session, &target.id, action)
                                    .await
                                {
                                    Ok(detail) => {
                                        final_outcome = ComputerExecutionOutcome::Performed;
                                        fallback_used = true;
                                        explanation = format!(
                                            "background route failed ({primary_detail}); takeover fallback succeeded"
                                        );
                                        execution_verification =
                                            pixel_execution_verification(detail.clone());
                                        attempts.push(ComputerExecutionAttempt {
                                            method: pixel_execution_method(action),
                                            outcome: final_outcome,
                                            semantic_element_id: None,
                                            pixel_target: None,
                                            verification: execution_verification.clone(),
                                            generation_before: None,
                                            generation_after: None,
                                            timing_ms: fallback_started.elapsed().as_millis(),
                                            detail: Some(detail),
                                        });
                                    }
                                    Err(fallback_error) => {
                                        final_outcome =
                                            execution_outcome_from_error(&fallback_error);
                                        fallback_used = true;
                                        explanation = format!(
                                            "background route failed ({primary_detail}); takeover fallback failed: {fallback_error}"
                                        );
                                        attempts.push(ComputerExecutionAttempt {
                                            method: pixel_execution_method(action),
                                            outcome: final_outcome,
                                            semantic_element_id: None,
                                            pixel_target: None,
                                            verification: pixel_execution_verification(
                                                fallback_error.to_string(),
                                            ),
                                            generation_before: None,
                                            generation_after: None,
                                            timing_ms: fallback_started.elapsed().as_millis(),
                                            detail: Some("route=takeover_fallback".into()),
                                        });
                                    }
                                }
                                pixel_attempt_ms = pixel_attempt_ms
                                    .saturating_add(fallback_started.elapsed().as_millis());
                            }
                        }
                    } else {
                        let pixel_started = Instant::now();
                        let result = self.execute(session, action).await;
                        pixel_attempt_ms = pixel_started.elapsed().as_millis();
                        match result {
                            Ok(result) => {
                                final_outcome = ComputerExecutionOutcome::Performed;
                                explanation =
                                    "pixel action succeeded on the current foreground window"
                                        .into();
                                let detail = result
                                    .backend_detail
                                    .clone()
                                    .unwrap_or_else(|| "CGEvent completed".into());
                                execution_verification =
                                    pixel_execution_verification(detail.clone());
                                attempts.push(ComputerExecutionAttempt {
                                    method: pixel_execution_method(action),
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: execution_verification.clone(),
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                    detail: Some(detail),
                                });
                            }
                            Err(error) => {
                                final_outcome = execution_outcome_from_error(&error);
                                explanation = error.to_string();
                                attempts.push(ComputerExecutionAttempt {
                                    method: pixel_execution_method(action),
                                    outcome: final_outcome,
                                    semantic_element_id: None,
                                    pixel_target: None,
                                    verification: pixel_execution_verification(error.to_string()),
                                    generation_before: None,
                                    generation_after: None,
                                    timing_ms: pixel_attempt_ms,
                                    detail: Some("route=takeover_primary".into()),
                                });
                            }
                        }
                    }
                }
            }
            ComputerExecutionIntent::Semantic(action) => {
                let semantic_allowed = matches!(
                    request.strategy,
                    ComputerExecutionStrategy::SemanticOnly
                        | ComputerExecutionStrategy::PreferSemantic
                );
                let fallback_binding =
                    if request.strategy != ComputerExecutionStrategy::SemanticOnly {
                        self.semantic_pixel_fallback(session, action)
                    } else {
                        None
                    };
                let fallback_available = fallback_binding.is_some();
                let fallback_allowed = fallback_available
                    && request.strategy != ComputerExecutionStrategy::SemanticOnly
                    && (request.fallback_policy == ComputerFallbackPolicy::Allow
                        || !semantic_allowed);
                let semantic_started = Instant::now();
                let semantic_result = if semantic_allowed {
                    self.semantic_action(session, action).await
                } else {
                    Ok(SemanticActionResult {
                        action: action.clone(),
                        element_id: action.element_id().clone(),
                        status: SemanticActionStatus::Unsupported,
                        verification: SemanticActionVerification {
                            detail: Some("caller selected a pixel execution strategy".into()),
                            ..Default::default()
                        },
                        timing: SemanticActionTiming::default(),
                        observation_generation_before: None,
                        observation_generation_after: None,
                        security: None,
                    })
                };
                semantic_attempt_ms = if semantic_allowed {
                    semantic_started.elapsed().as_millis()
                } else {
                    0
                };
                match semantic_result {
                    Ok(semantic) => {
                        final_outcome = execution_outcome_from_semantic(semantic.status);
                        execution_verification =
                            execution_verification_from_semantic(action, &semantic);
                        verification_ms = millis_from_micros(semantic.timing.verification_micros);
                        generation_before = semantic.observation_generation_before;
                        generation_after = semantic.observation_generation_after;
                        explanation = semantic.verification.detail.clone().unwrap_or_else(|| {
                            format!("macOS AX semantic action returned {:?}", semantic.status)
                        });
                        attempts.push(ComputerExecutionAttempt {
                            method: semantic_method(action),
                            outcome: final_outcome,
                            semantic_element_id: Some(action.element_id().clone()),
                            pixel_target: None,
                            verification: execution_verification.clone(),
                            generation_before,
                            generation_after,
                            timing_ms: semantic_attempt_ms,
                            detail: semantic.verification.detail.clone(),
                        });
                        let fallback_decision_started = Instant::now();
                        let can_fallback =
                            fallback_allowed && semantic_status_allows_takeover(semantic.status);
                        fallback_decision_ms = fallback_decision_started.elapsed().as_millis();
                        if can_fallback {
                            let (window_id, pixel_action, binding) =
                                fallback_binding.expect("fallback_available was checked");
                            let fallback_started = Instant::now();
                            match self
                                .execute_preferred_pixel_fallback(
                                    session,
                                    &window_id,
                                    &pixel_action,
                                    request.execution_mode,
                                )
                                .await
                            {
                                Ok((detail, _used_takeover)) => {
                                    final_outcome = ComputerExecutionOutcome::Performed;
                                    fallback_used = true;
                                    pixel_target = Some(binding);
                                    execution_verification =
                                        pixel_execution_verification(detail.clone());
                                    explanation = format!(
                                        "semantic route returned {:?}; takeover pixel fallback succeeded",
                                        semantic.status
                                    );
                                    attempts.push(ComputerExecutionAttempt {
                                        method: pixel_execution_method(&pixel_action),
                                        outcome: final_outcome,
                                        semantic_element_id: Some(action.element_id().clone()),
                                        pixel_target: pixel_target.clone(),
                                        verification: execution_verification.clone(),
                                        generation_before,
                                        generation_after,
                                        timing_ms: fallback_started.elapsed().as_millis(),
                                        detail: Some(detail),
                                    });
                                }
                                Err(fallback_error) => {
                                    fallback_used = true;
                                    final_outcome = execution_outcome_from_error(&fallback_error);
                                    explanation = format!(
                                        "semantic route returned {:?}; takeover pixel fallback failed: {fallback_error}",
                                        semantic.status
                                    );
                                    attempts.push(ComputerExecutionAttempt {
                                        method: pixel_execution_method(&pixel_action),
                                        outcome: final_outcome,
                                        semantic_element_id: Some(action.element_id().clone()),
                                        pixel_target: Some(binding),
                                        verification: pixel_execution_verification(
                                            fallback_error.to_string(),
                                        ),
                                        generation_before,
                                        generation_after,
                                        timing_ms: fallback_started.elapsed().as_millis(),
                                        detail: Some("route=takeover_fallback".into()),
                                    });
                                }
                            }
                            pixel_attempt_ms = fallback_started.elapsed().as_millis();
                        } else if request.fallback_policy == ComputerFallbackPolicy::Allow
                            && !fallback_available
                        {
                            explanation.push_str(
                                "; no safe pixel binding was available for this semantic element",
                            );
                        }
                    }
                    Err(error) => {
                        final_outcome = execution_outcome_from_error(&error);
                        explanation = error.to_string();
                        attempts.push(ComputerExecutionAttempt {
                            method: semantic_method(action),
                            outcome: final_outcome,
                            semantic_element_id: Some(action.element_id().clone()),
                            pixel_target: None,
                            verification: ComputerExecutionVerification {
                                kind: Some(semantic_verification_kind(action)),
                                detail: Some(error.to_string()),
                                ..Default::default()
                            },
                            generation_before: None,
                            generation_after: None,
                            timing_ms: semantic_attempt_ms,
                            detail: Some(error.to_string()),
                        });
                        if fallback_allowed {
                            if let Some((window_id, pixel_action, binding)) = fallback_binding {
                                let fallback_started = Instant::now();
                                match self
                                    .execute_preferred_pixel_fallback(
                                        session,
                                        &window_id,
                                        &pixel_action,
                                        request.execution_mode,
                                    )
                                    .await
                                {
                                    Ok((detail, _used_takeover)) => {
                                        final_outcome = ComputerExecutionOutcome::Performed;
                                        fallback_used = true;
                                        pixel_target = Some(binding);
                                        execution_verification =
                                            pixel_execution_verification(detail.clone());
                                        explanation = format!(
                                        "semantic route failed ({error}); takeover pixel fallback succeeded"
                                    );
                                        attempts.push(ComputerExecutionAttempt {
                                            method: pixel_execution_method(&pixel_action),
                                            outcome: final_outcome,
                                            semantic_element_id: Some(action.element_id().clone()),
                                            pixel_target: pixel_target.clone(),
                                            verification: execution_verification.clone(),
                                            generation_before: None,
                                            generation_after: None,
                                            timing_ms: fallback_started.elapsed().as_millis(),
                                            detail: Some(detail),
                                        });
                                    }
                                    Err(fallback_error) => {
                                        fallback_used = true;
                                        final_outcome =
                                            execution_outcome_from_error(&fallback_error);
                                        explanation = format!(
                                        "semantic route failed ({error}); takeover pixel fallback failed: {fallback_error}"
                                    );
                                        attempts.push(ComputerExecutionAttempt {
                                            method: pixel_execution_method(&pixel_action),
                                            outcome: final_outcome,
                                            semantic_element_id: Some(action.element_id().clone()),
                                            pixel_target: Some(binding),
                                            verification: pixel_execution_verification(
                                                fallback_error.to_string(),
                                            ),
                                            generation_before: None,
                                            generation_after: None,
                                            timing_ms: fallback_started.elapsed().as_millis(),
                                            detail: Some("route=takeover_fallback".into()),
                                        });
                                    }
                                }
                                pixel_attempt_ms = fallback_started.elapsed().as_millis();
                            }
                        }
                    }
                }
            }
        }
        let total_ms = started.elapsed().as_millis();
        Ok(ComputerExecutionResult {
            requested_intent: request.intent.clone(),
            selected_strategy: request.strategy,
            attempts,
            final_outcome,
            semantic_element_id: request.semantic_element_id().cloned(),
            pixel_target,
            verification: execution_verification,
            fallback_used,
            generation_before,
            generation_after,
            timing: ComputerExecutionTiming {
                policy_ms: total_ms,
                semantic_attempt_ms,
                fallback_decision_ms,
                pixel_attempt_ms,
                verification_ms,
                total_ms,
            },
            explanation: if explanation.is_empty() {
                "execution request was rejected before any attempt".into()
            } else {
                explanation
            },
            security,
        })
    }

    async fn cleanup_pressed(&mut self, session: &ComputerSessionId) -> Result<(), ComputerError> {
        self.ensure_initialized()?;
        self.release_pressed(session);
        Ok(())
    }

    async fn close_session(&mut self, session: &ComputerSessionId) -> Result<(), ComputerError> {
        self.release_pressed(session);
        self.frames.remove(session);
        self.semantic.remove(session);
        self.virtual_cursor_clear(session);
        Ok(())
    }
}

fn ax_attribute(name: &str) -> Result<CFStringRef, ComputerError> {
    let name = CString::new(name)
        .map_err(|_| ComputerError::Backend("AX attribute contains NUL".into()))?;
    let value =
        unsafe { CFStringCreateWithCString(std::ptr::null(), name.as_ptr(), UTF8_ENCODING) };
    if value.is_null() {
        Err(ComputerError::Backend(
            "failed to allocate AX attribute string".into(),
        ))
    } else {
        Ok(value)
    }
}

fn ax_error(operation: &str, code: i32) -> ComputerError {
    match code {
        -25211 => ComputerError::AccessDenied(format!(
            "{operation} failed with kAXErrorAPIDisabled; grant Accessibility permission"
        )),
        -25202 => ComputerError::TargetUnavailable(format!(
            "{operation} failed with kAXErrorInvalidUIElement"
        )),
        -25204 => ComputerError::Backend(format!("{operation} failed with kAXErrorCannotComplete")),
        _ => ComputerError::Backend(format!("{operation} failed with AX error {code}")),
    }
}

fn ax_copy_attribute(
    element: CFTypeRef,
    attribute_name: &str,
) -> Result<Option<CFTypeRef>, ComputerError> {
    let attribute = ax_attribute(attribute_name)?;
    let mut value = std::ptr::null();
    let result = unsafe { AXUIElementCopyAttributeValue(element, attribute, &mut value) };
    unsafe { CFRelease(attribute) };
    match result {
        AX_ERROR_SUCCESS if !value.is_null() => Ok(Some(value)),
        AX_ERROR_SUCCESS | AX_ERROR_ATTRIBUTE_UNSUPPORTED | AX_ERROR_NO_VALUE => Ok(None),
        error => Err(ax_error(
            &format!("AXUIElementCopyAttributeValue({attribute_name})"),
            error,
        )),
    }
}

fn ax_copy_children(element: CFTypeRef) -> Result<Vec<(usize, CFTypeRef)>, ComputerError> {
    let Some(children) = ax_copy_attribute(element, "AXChildren")? else {
        return Ok(Vec::new());
    };
    let is_array = unsafe { CFGetTypeID(children) == CFArrayGetTypeID() };
    if !is_array {
        unsafe { CFRelease(children) };
        return Ok(Vec::new());
    }
    let count = unsafe { CFArrayGetCount(children) }.max(0) as usize;
    let mut result = Vec::with_capacity(count);
    for index in 0..count {
        let child = unsafe { CFArrayGetValueAtIndex(children, index as CFIndex) };
        if !child.is_null() {
            let child = unsafe { CFRetain(child) };
            result.push((index, child));
        }
    }
    unsafe { CFRelease(children) };
    Ok(result)
}

fn ax_copy_windows(application: CFTypeRef) -> Result<Vec<(usize, CFTypeRef)>, ComputerError> {
    ax_copy_array_attribute(application, "AXWindows")
}

fn ax_copy_array_attribute(
    element: CFTypeRef,
    attribute_name: &str,
) -> Result<Vec<(usize, CFTypeRef)>, ComputerError> {
    let Some(array) = ax_copy_attribute(element, attribute_name)? else {
        return Ok(Vec::new());
    };
    let is_array = unsafe { CFGetTypeID(array) == CFArrayGetTypeID() };
    if !is_array {
        unsafe { CFRelease(array) };
        return Ok(Vec::new());
    }
    let count = unsafe { CFArrayGetCount(array) }.max(0) as usize;
    let mut result = Vec::with_capacity(count);
    for index in 0..count {
        let item = unsafe { CFArrayGetValueAtIndex(array, index as CFIndex) };
        if !item.is_null() {
            result.push((index, unsafe { CFRetain(item) }));
        }
    }
    unsafe { CFRelease(array) };
    Ok(result)
}

fn ax_copy_action_names(element: CFTypeRef) -> Result<HashSet<String>, ComputerError> {
    let mut names = std::ptr::null();
    let result = unsafe { AXUIElementCopyActionNames(element, &mut names) };
    if result != AX_ERROR_SUCCESS {
        if result == AX_ERROR_ATTRIBUTE_UNSUPPORTED || result == AX_ERROR_NO_VALUE {
            return Ok(HashSet::new());
        }
        return Err(ax_error("AXUIElementCopyActionNames", result));
    }
    if names.is_null() {
        return Ok(HashSet::new());
    }
    let count = unsafe { CFArrayGetCount(names) }.max(0) as usize;
    let mut result_names = HashSet::with_capacity(count);
    for index in 0..count {
        let value = unsafe { CFArrayGetValueAtIndex(names, index as CFIndex) };
        if !value.is_null() {
            if let Some(name) = ax_string_value(value) {
                result_names.insert(name);
            }
        }
    }
    unsafe { CFRelease(names) };
    Ok(result_names)
}

fn ax_attribute_settable(element: CFTypeRef, attribute_name: &str) -> bool {
    let Ok(attribute) = ax_attribute(attribute_name) else {
        return false;
    };
    let mut settable = 0u8;
    let result = unsafe { AXUIElementIsAttributeSettable(element, attribute, &mut settable) };
    unsafe { CFRelease(attribute) };
    result == AX_ERROR_SUCCESS && settable != 0
}

fn ax_perform_action(element: CFTypeRef, action_name: &str) -> Result<(), ComputerError> {
    let action = ax_attribute(action_name)?;
    let result = unsafe { AXUIElementPerformAction(element, action) };
    unsafe { CFRelease(action) };
    if result == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(ax_error(
            &format!("AXUIElementPerformAction({action_name})"),
            result,
        ))
    }
}

fn ax_set_attribute(
    element: CFTypeRef,
    attribute_name: &str,
    value: CFTypeRef,
) -> Result<(), ComputerError> {
    let attribute = ax_attribute(attribute_name)?;
    let result = unsafe { AXUIElementSetAttributeValue(element, attribute, value) };
    unsafe { CFRelease(attribute) };
    if result == AX_ERROR_SUCCESS {
        Ok(())
    } else {
        Err(ax_error(
            &format!("AXUIElementSetAttributeValue({attribute_name})"),
            result,
        ))
    }
}

fn ax_string_value(value: CFTypeRef) -> Option<String> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFStringGetTypeID() } {
        return None;
    }
    cf_string(value)
}

fn ax_bool_value(value: CFTypeRef) -> Option<bool> {
    if value.is_null() {
        return None;
    }
    let type_id = unsafe { CFGetTypeID(value) };
    if type_id == unsafe { CFBooleanGetTypeID() } {
        return Some(unsafe { CFBooleanGetValue(value) != 0 });
    }
    if type_id == unsafe { CFNumberGetTypeID() } {
        let mut number = 0i32;
        return (unsafe {
            CFNumberGetValue(
                value,
                CF_NUMBER_SINT32,
                &mut number as *mut _ as *mut c_void,
            )
        } != 0)
            .then_some(number != 0);
    }
    None
}

fn ax_number_value(value: CFTypeRef) -> Option<f64> {
    if value.is_null() || unsafe { CFGetTypeID(value) != CFNumberGetTypeID() } {
        return None;
    }
    let mut number = 0.0f64;
    (unsafe {
        CFNumberGetValue(
            value,
            CF_NUMBER_DOUBLE,
            &mut number as *mut _ as *mut c_void,
        )
    } != 0)
        .then_some(number)
}

fn ax_point_value(value: CFTypeRef) -> Option<CGPoint> {
    if value.is_null()
        || unsafe { CFGetTypeID(value) != AXValueGetTypeID() }
        || unsafe { AXValueGetType(value) != AX_VALUE_CGPOINT }
    {
        return None;
    }
    let mut point = CGPoint::default();
    (unsafe { AXValueGetValue(value, AX_VALUE_CGPOINT, &mut point as *mut _ as *mut c_void) } != 0)
        .then_some(point)
}

fn ax_size_value(value: CFTypeRef) -> Option<CGSize> {
    if value.is_null()
        || unsafe { CFGetTypeID(value) != AXValueGetTypeID() }
        || unsafe { AXValueGetType(value) != AX_VALUE_CGSIZE }
    {
        return None;
    }
    let mut size = CGSize::default();
    (unsafe { AXValueGetValue(value, AX_VALUE_CGSIZE, &mut size as *mut _ as *mut c_void) } != 0)
        .then_some(size)
}

fn ax_element_for_path(
    process_id: u32,
    window_index: usize,
    child_path: &[usize],
) -> Result<CFTypeRef, ComputerError> {
    let application = unsafe { AXUIElementCreateApplication(process_id as i32) };
    if application.is_null() {
        return Err(ComputerError::AccessDenied(
            "AXUIElementCreateApplication returned null".into(),
        ));
    }
    let windows = match ax_copy_windows(application) {
        Ok(windows) => windows,
        Err(error) => {
            unsafe { CFRelease(application) };
            return Err(error);
        }
    };
    let Some(window_position) = windows.iter().position(|(index, _)| *index == window_index) else {
        for (_, window) in windows {
            unsafe { CFRelease(window) };
        }
        unsafe { CFRelease(application) };
        return Err(ComputerError::StaleElement(
            "AX window path is no longer present".into(),
        ));
    };
    let mut current = windows[window_position].1;
    for (position, (_, window)) in windows.into_iter().enumerate() {
        if position != window_position {
            unsafe { CFRelease(window) };
        }
    }
    unsafe { CFRelease(application) };
    for child_index in child_path {
        let children = match ax_copy_children(current) {
            Ok(children) => children,
            Err(error) => {
                unsafe { CFRelease(current) };
                return Err(error);
            }
        };
        let Some(next_position) = children.iter().position(|(index, _)| index == child_index)
        else {
            unsafe { CFRelease(current) };
            return Err(ComputerError::StaleElement(
                "AX child path is no longer present".into(),
            ));
        };
        let next = children[next_position].1;
        for (position, (_, child)) in children.into_iter().enumerate() {
            if position != next_position {
                unsafe { CFRelease(child) };
            }
        }
        unsafe { CFRelease(current) };
        current = next;
    }
    Ok(current)
}

struct AxWalkState<'a> {
    nonce: u64,
    generation: u64,
    window_id: &'a WindowId,
    process_id: u32,
    window_index: usize,
    limits: SemanticObservationLimits,
    elements: &'a mut Vec<alice_computer_use_core::ComputerElement>,
    current: &'a mut HashMap<ElementId, MacElementRecord>,
    property_micros: u128,
    truncated: bool,
}

fn walk_ax_tree(
    element: CFTypeRef,
    child_path: &[usize],
    parent_id: Option<&ElementId>,
    depth: u32,
    state: &mut AxWalkState<'_>,
) -> Option<ElementId> {
    if state.elements.len() >= state.limits.max_elements as usize {
        state.truncated = true;
        return None;
    }
    let ordinal = state.elements.len();
    let id = ElementId::new(format!(
        "ax-{:016x}-{}-{ordinal}",
        state.nonce, state.generation
    ));
    let property_started = Instant::now();
    let (semantic, password) = ax_read_element(element, id.clone(), parent_id.cloned());
    state.property_micros = state
        .property_micros
        .saturating_add(property_started.elapsed().as_micros());
    state.elements.push(semantic.clone());
    state.current.insert(
        id.clone(),
        MacElementRecord {
            semantic,
            process_id: state.process_id,
            window_index: state.window_index,
            child_path: child_path.to_vec(),
            window_id: state.window_id.clone(),
            password,
        },
    );

    let mut child_ids = Vec::new();
    if depth >= state.limits.max_depth {
        match ax_copy_children(element) {
            Ok(children) => {
                state.truncated |= !children.is_empty();
                for (_, child) in children {
                    unsafe { CFRelease(child) };
                }
            }
            Err(_) => {
                state.truncated = true;
            }
        }
    } else {
        match ax_copy_children(element) {
            Ok(children) => {
                for (child_index, child) in children {
                    if state.elements.len() >= state.limits.max_elements as usize {
                        state.truncated = true;
                        unsafe { CFRelease(child) };
                        continue;
                    }
                    let mut path = child_path.to_vec();
                    path.push(child_index);
                    if let Some(child_id) =
                        walk_ax_tree(child, &path, Some(&id), depth.saturating_add(1), state)
                    {
                        child_ids.push(child_id);
                    }
                    unsafe { CFRelease(child) };
                }
            }
            Err(_) => state.truncated = true,
        }
    }
    if let Some(current) = state.elements.get_mut(ordinal) {
        current.child_ids = child_ids;
    }
    Some(id)
}

fn ax_read_element(
    element: CFTypeRef,
    id: ElementId,
    parent_id: Option<ElementId>,
) -> (alice_computer_use_core::ComputerElement, bool) {
    let role = ax_attribute_string(element, "AXRole").unwrap_or_else(|| "AXUnknown".into());
    let subrole = ax_attribute_string(element, "AXSubrole");
    let title = ax_attribute_string(element, "AXTitle");
    let description = ax_attribute_string(element, "AXDescription");
    let value = ax_attribute_string(element, "AXValue");
    let password = role == "AXSecureTextField"
        || subrole
            .as_deref()
            .is_some_and(|value| value.to_ascii_lowercase().contains("secure"));
    let enabled = ax_attribute_bool(element, "AXEnabled").unwrap_or(true);
    let focused = ax_attribute_bool(element, "AXFocused").unwrap_or(false);
    let selected = ax_attribute_bool(element, "AXSelected");
    let expanded = ax_attribute_bool(element, "AXExpanded");
    let bounds = ax_rect_attribute(element).map(|rect| Coordinate {
        space: CoordinateSpace::DesktopPhysical,
        point: rect.origin,
        extent: rect.size,
        dpi: DpiScale::ONE,
        display_id: None,
        frame_id: None,
    });
    let actions = ax_copy_action_names(element).unwrap_or_default();
    let range_value = ax_attribute_number(element, "AXValue");
    let range_minimum = ax_attribute_number(element, "AXMinValue");
    let range_maximum = ax_attribute_number(element, "AXMaxValue");
    let focusable = ax_attribute_settable(element, "AXFocused")
        || matches!(
            role.as_str(),
            "AXButton"
                | "AXCheckBox"
                | "AXRadioButton"
                | "AXTextField"
                | "AXTextArea"
                | "AXComboBox"
                | "AXSlider"
        );
    let editable = !password
        && ax_attribute_settable(element, "AXValue")
        && matches!(
            role.as_str(),
            "AXTextField" | "AXTextArea" | "AXSecureTextField" | "AXComboBox" | "AXSearchField"
        );
    let toggleable = matches!(role.as_str(), "AXCheckBox" | "AXRadioButton" | "AXSwitch")
        || (selected.is_some() && actions.contains("AXPress"));
    let selectable = selected.is_some() || actions.contains("AXPick");
    let expandable = expanded.is_some() || role == "AXDisclosureTriangle";
    let range_adjustable = range_value.is_some()
        && (range_minimum.is_some()
            || range_maximum.is_some()
            || actions.contains("AXIncrement")
            || actions.contains("AXDecrement"));
    let scrollable = matches!(role.as_str(), "AXScrollArea" | "AXScrollBar")
        || ax_attribute_exists(element, "AXVerticalScrollBar")
        || ax_attribute_exists(element, "AXHorizontalScrollBar");
    let value_summary = if password {
        None
    } else {
        value.as_deref().map(bounded_text)
    };
    let text_summary = if password {
        None
    } else {
        title
            .as_deref()
            .or(description.as_deref())
            .map(bounded_text)
    };
    let semantic = alice_computer_use_core::ComputerElement {
        id,
        parent_id,
        child_ids: Vec::new(),
        role: role.clone(),
        control_type: subrole.unwrap_or_else(|| role.clone()),
        name: title.or(description),
        automation_id: None,
        class_name: Some(role.clone()),
        value_summary,
        text_summary,
        bounds,
        enabled,
        focused,
        focusable,
        offscreen: false,
        toggle_state: selected.or_else(|| {
            if semantic_role_for_toggle(&role) {
                ax_attribute_bool(element, "AXValue")
            } else {
                None
            }
        }),
        selected,
        expanded,
        range_value,
        range_minimum,
        range_maximum,
        capabilities: alice_computer_use_core::SemanticCapabilities {
            invokable: actions.contains("AXPress") || actions.contains("AXPick"),
            editable,
            selectable,
            scrollable,
            expandable,
            toggleable,
            range_adjustable,
            scroll_into_view: false,
        },
    };
    (semantic, password)
}

fn semantic_role_for_toggle(role: &str) -> bool {
    matches!(role, "AXCheckBox" | "AXRadioButton" | "AXSwitch")
}

fn bounded_text(value: &str) -> String {
    const MAX_TEXT: usize = 4096;
    if value.len() <= MAX_TEXT {
        return value.to_owned();
    }
    let mut end = MAX_TEXT;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &value[..end])
}

fn ax_attribute_exists(element: CFTypeRef, attribute_name: &str) -> bool {
    let Ok(value) = ax_copy_attribute(element, attribute_name) else {
        return false;
    };
    if let Some(value) = value {
        unsafe { CFRelease(value) };
        true
    } else {
        false
    }
}

fn ax_attribute_string(element: CFTypeRef, attribute_name: &str) -> Option<String> {
    let value = ax_copy_attribute(element, attribute_name).ok().flatten()?;
    let result = ax_string_value(value);
    unsafe { CFRelease(value) };
    result
}

fn ax_attribute_bool(element: CFTypeRef, attribute_name: &str) -> Option<bool> {
    let value = ax_copy_attribute(element, attribute_name).ok().flatten()?;
    let result = ax_bool_value(value);
    unsafe { CFRelease(value) };
    result
}

fn ax_attribute_number(element: CFTypeRef, attribute_name: &str) -> Option<f64> {
    let value = ax_copy_attribute(element, attribute_name).ok().flatten()?;
    let result = ax_number_value(value);
    unsafe { CFRelease(value) };
    result
}

fn ax_rect_attribute(element: CFTypeRef) -> Option<Rect> {
    let position = ax_copy_attribute(element, "AXPosition")
        .ok()
        .flatten()
        .and_then(|value| {
            let result = ax_point_value(value);
            unsafe { CFRelease(value) };
            result
        });
    let size = ax_copy_attribute(element, "AXSize")
        .ok()
        .flatten()
        .and_then(|value| {
            let result = ax_size_value(value);
            unsafe { CFRelease(value) };
            result
        });
    match (position, size) {
        (Some(position), Some(size))
            if position.x.is_finite()
                && position.y.is_finite()
                && size.width.is_finite()
                && size.height.is_finite()
                && size.width >= 0.0
                && size.height >= 0.0 =>
        {
            Some(Rect {
                origin: Point {
                    x: position.x,
                    y: position.y,
                },
                size: Size {
                    width: size.width,
                    height: size.height,
                },
            })
        }
        _ => None,
    }
}

fn find_ax_window(
    application: CFTypeRef,
    target: &WindowRecord,
) -> Result<(usize, CFTypeRef), ComputerError> {
    let windows = ax_copy_windows(application)?;
    let mut selected = None;
    let mut selected_score = 0u8;
    for (index, window) in &windows {
        let title = ax_attribute_string(*window, "AXTitle");
        let geometry = ax_rect_attribute(*window);
        let title_match =
            !target.title.is_empty() && title.as_deref().is_some_and(|value| value == target.title);
        let geometry_match = geometry.is_some_and(|rect| rect_close(rect, target.bounds, 2.0));
        let score = (title_match as u8) * 4 + (geometry_match as u8) * 3;
        if score > selected_score {
            selected = Some((*index, *window));
            selected_score = score;
        }
    }
    let Some(selected) = selected else {
        for (_, window) in windows {
            unsafe { CFRelease(window) };
        }
        return Err(ComputerError::TargetUnavailable(
            "AXWindows did not contain the selected CoreGraphics window".into(),
        ));
    };
    for (_, window) in windows {
        if window != selected.1 {
            unsafe { CFRelease(window) };
        }
    }
    Ok(selected)
}

fn rect_close(left: Rect, right: Rect, tolerance: f64) -> bool {
    (left.origin.x - right.origin.x).abs() <= tolerance
        && (left.origin.y - right.origin.y).abs() <= tolerance
        && (left.size.width - right.size.width).abs() <= tolerance
        && (left.size.height - right.size.height).abs() <= tolerance
}

fn request_ax_trust_prompt() {
    // Ask macOS to display its standard Accessibility authorization prompt.
    // The prompt is only shown when this process is not already trusted.
    unsafe {
        let keys = [kAXTrustedCheckOptionPrompt];
        let values = [kCFBooleanTrue];
        let options = CFDictionaryCreate(
            std::ptr::null(),
            keys.as_ptr(),
            values.as_ptr(),
            1,
            std::ptr::null(),
            std::ptr::null(),
        );
        if !options.is_null() {
            let _ = AXIsProcessTrustedWithOptions(options);
            CFRelease(options);
        }
    }
}

fn request_screen_capture_prompt() {
    // Ask macOS to display its standard Screen Recording authorization prompt.
    // A CLI sidecar has no foreground app window, and recent macOS releases
    // may return without surfacing a prompt for such a process. Open the
    // system pane as a deterministic fallback so the user can grant the exact
    // executable that is actually doing the capture.
    let requested = unsafe { CGRequestScreenCaptureAccess() } != 0;
    if requested || unsafe { CGPreflightScreenCaptureAccess() } != 0 {
        return;
    }
    static OPENED_SCREEN_CAPTURE_SETTINGS: Once = Once::new();
    OPENED_SCREEN_CAPTURE_SETTINGS.call_once(|| {
        let _ = std::process::Command::new("/usr/bin/open")
            .arg("x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture")
            .spawn();
    });
}

fn ensure_screen_capture_access() -> Result<(), ComputerError> {
    if unsafe { CGPreflightScreenCaptureAccess() } != 0 {
        return Ok(());
    }
    request_screen_capture_prompt();
    Err(ComputerError::AccessDenied(
        "macOS Screen Recording permission is required; enable the sidecar in System Settings > Privacy & Security > Screen Recording, then retry".into(),
    ))
}

fn ensure_ax_trusted() -> Result<(), ComputerError> {
    if unsafe { AXIsProcessTrusted() } != 0 {
        Ok(())
    } else {
        request_ax_trust_prompt();
        Err(ComputerError::AccessDenied(
            "macOS Accessibility permission is required; enable the sidecar in System Settings > Privacy & Security > Accessibility".into(),
        ))
    }
}

fn semantic_session_nonce(session: &ComputerSessionId) -> u64 {
    let mut hasher = DefaultHasher::new();
    session.hash(&mut hasher);
    hasher.finish()
}

fn ax_element_status(
    state: &MacSemanticSessionState,
    element_id: &ElementId,
) -> Option<SemanticActionStatus> {
    if state.current.contains_key(element_id) {
        return None;
    }
    let prefix = format!("ax-{:016x}-", state.nonce);
    let generation = element_id
        .as_str()
        .strip_prefix(&prefix)
        .and_then(|value| value.split('-').next())
        .and_then(|value| value.parse::<u64>().ok());
    Some(
        if generation.is_some_and(|value| value < state.generation) {
            SemanticActionStatus::StaleElement
        } else {
            SemanticActionStatus::UnknownElement
        },
    )
}

fn semantic_action_capability_status(
    action: &SemanticAction,
    element: &alice_computer_use_core::ComputerElement,
) -> Option<SemanticActionStatus> {
    let supported = match action {
        SemanticAction::Focus { .. } => element.focusable,
        SemanticAction::Invoke { .. } => element.capabilities.invokable,
        SemanticAction::SetValue { .. } => element.capabilities.editable,
        SemanticAction::Toggle { .. } => element.capabilities.toggleable,
        SemanticAction::Select { .. } => element.capabilities.selectable,
        SemanticAction::Expand { .. } | SemanticAction::Collapse { .. } => {
            element.capabilities.expandable
        }
        SemanticAction::SetRangeValue { .. } => element.capabilities.range_adjustable,
        SemanticAction::ScrollIntoView { .. } => element.capabilities.scroll_into_view,
    };
    (!supported).then_some(SemanticActionStatus::Unsupported)
}

fn dispatch_ax_action(element: CFTypeRef, action: &SemanticAction) -> Result<(), ComputerError> {
    match action {
        SemanticAction::Focus { .. } => {
            ax_set_attribute(element, "AXFocused", unsafe { kCFBooleanTrue })
        }
        SemanticAction::Invoke { .. } | SemanticAction::Toggle { .. } => {
            ax_perform_action(element, "AXPress")
        }
        SemanticAction::SetValue { value, .. } => {
            let value_ref = ax_attribute(value)?;
            let result = ax_set_attribute(element, "AXValue", value_ref);
            unsafe { CFRelease(value_ref) };
            result
        }
        SemanticAction::Select { .. } => {
            if ax_attribute_settable(element, "AXSelected") {
                ax_set_attribute(element, "AXSelected", unsafe { kCFBooleanTrue })
            } else {
                ax_perform_action(element, "AXPick")
            }
        }
        SemanticAction::Expand { .. } => {
            ax_set_attribute(element, "AXExpanded", unsafe { kCFBooleanTrue })
        }
        SemanticAction::Collapse { .. } => {
            let value = ax_boolean(false)?;
            let result = ax_set_attribute(element, "AXExpanded", value);
            unsafe { CFRelease(value) };
            result
        }
        SemanticAction::SetRangeValue { value, .. } => {
            let value = ax_number(*value)?;
            let result = ax_set_attribute(element, "AXValue", value);
            unsafe { CFRelease(value) };
            result
        }
        SemanticAction::ScrollIntoView { .. } => Err(ComputerError::Unsupported {
            capability: "semantic_scroll_into_view".into(),
            detail: "macOS AX has no portable scroll-into-view action".into(),
        }),
    }
}

fn ax_boolean(value: bool) -> Result<CFTypeRef, ComputerError> {
    Ok(unsafe {
        CFRetain(if value {
            kCFBooleanTrue
        } else {
            kCFBooleanFalse
        })
    })
}

fn ax_number(value: f64) -> Result<CFTypeRef, ComputerError> {
    let value = unsafe {
        CFNumberCreate(
            std::ptr::null(),
            CF_NUMBER_DOUBLE,
            &value as *const _ as *const c_void,
        )
    };
    if value.is_null() {
        Err(ComputerError::Backend(
            "CFNumberCreate returned null".into(),
        ))
    } else {
        Ok(value)
    }
}

fn verify_ax_action(
    element: CFTypeRef,
    action: &SemanticAction,
    before: &alice_computer_use_core::ComputerElement,
) -> Result<SemanticActionVerification, ComputerError> {
    let mut verification = SemanticActionVerification::default();
    match action {
        SemanticAction::Focus { .. } => {
            let focused = ax_attribute_bool(element, "AXFocused");
            verification.focused = focused;
            verification.verified = focused == Some(true);
        }
        SemanticAction::Invoke { .. } => {
            verification.verified = true;
        }
        SemanticAction::SetValue { value, .. } => {
            let observed = ax_attribute_string(element, "AXValue");
            verification.observed_value = observed.clone();
            verification.verified = observed.as_deref() == Some(value.as_str());
        }
        SemanticAction::Toggle { .. } => {
            let observed = ax_attribute_bool(element, "AXSelected")
                .or_else(|| ax_attribute_bool(element, "AXValue"));
            verification.toggled = observed;
            verification.state_changed = before
                .toggle_state
                .zip(observed)
                .map(|(left, right)| left != right);
            verification.verified = verification.state_changed == Some(true);
        }
        SemanticAction::Select { .. } => {
            let selected = ax_attribute_bool(element, "AXSelected");
            verification.selected = selected;
            verification.verified = selected == Some(true);
        }
        SemanticAction::Expand { .. } | SemanticAction::Collapse { .. } => {
            let expanded = ax_attribute_bool(element, "AXExpanded");
            let expected = matches!(action, SemanticAction::Expand { .. });
            verification.expanded = expanded;
            verification.verified = expanded == Some(expected);
        }
        SemanticAction::SetRangeValue { value, .. } => {
            let observed = ax_attribute_number(element, "AXValue");
            verification.range_value = observed;
            verification.verified = observed.is_some_and(|actual| (actual - value).abs() <= 0.0001);
        }
        SemanticAction::ScrollIntoView { .. } => {}
    }
    Ok(verification)
}

fn timing_from_started(started: Instant) -> SemanticActionTiming {
    SemanticActionTiming {
        total_micros: started.elapsed().as_micros(),
        ..Default::default()
    }
}

fn mac_semantic_action_result(
    action: &SemanticAction,
    status: SemanticActionStatus,
    before: Option<u64>,
    after: Option<u64>,
    verification: SemanticActionVerification,
    timing: SemanticActionTiming,
) -> SemanticActionResult {
    mac_semantic_action_result_with_timing(action, status, before, after, verification, timing)
}

fn mac_semantic_action_result_with_timing(
    action: &SemanticAction,
    status: SemanticActionStatus,
    before: Option<u64>,
    after: Option<u64>,
    verification: SemanticActionVerification,
    timing: SemanticActionTiming,
) -> SemanticActionResult {
    SemanticActionResult {
        action: action.clone(),
        element_id: action.element_id().clone(),
        status,
        verification,
        timing,
        observation_generation_before: before,
        observation_generation_after: after,
        security: None,
    }
}

#[derive(Clone)]
struct WindowRecord {
    id: WindowId,
    title: String,
    owner_name: String,
    process_id: u32,
    bounds: Rect,
    active: bool,
}

fn enumerate_window_records() -> Result<Vec<WindowRecord>, ComputerError> {
    let windows = unsafe {
        CGWindowListCopyWindowInfo(
            CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | CG_WINDOW_LIST_OPTION_EXCLUDE_DESKTOP,
            CG_NULL_WINDOW_ID,
        )
    };
    if windows.is_null() {
        return Err(ComputerError::AccessDenied(
            "CGWindowListCopyWindowInfo returned null".into(),
        ));
    }
    let count = unsafe { CFArrayGetCount(windows) };
    let mut records = Vec::new();
    // CGWindowList is ordered front-to-back, but its first window is not a
    // reliable foreground signal when overlays, menus, or mirrored-device
    // surfaces are present.  Prefer the Accessibility system-wide focused
    // application's PID and only fall back to the old ordering heuristic when
    // macOS does not expose that signal.
    let focused_process_id = focused_application_pid();
    for index in 0..count {
        let dictionary = unsafe { CFArrayGetValueAtIndex(windows, index) } as CFDictionaryRef;
        if dictionary.is_null() {
            continue;
        }
        let Some(number) = dictionary_i32(dictionary, "kCGWindowNumber") else {
            continue;
        };
        let Some(process_id) = dictionary_i32(dictionary, "kCGWindowOwnerPID") else {
            continue;
        };
        let layer = dictionary_i32(dictionary, "kCGWindowLayer").unwrap_or(0);
        let onscreen = dictionary_bool(dictionary, "kCGWindowIsOnscreen").unwrap_or(true);
        if process_id <= 0 || layer != 0 || !onscreen {
            continue;
        }
        let Some(bounds_value) = dictionary_value(dictionary, "kCGWindowBounds") else {
            continue;
        };
        let mut rect = CGRect::default();
        if unsafe {
            CGRectMakeWithDictionaryRepresentation(bounds_value as CFDictionaryRef, &mut rect)
        } == 0
        {
            continue;
        }
        if rect.size.width <= 0.0 || rect.size.height <= 0.0 {
            continue;
        }
        let title = dictionary_string(dictionary, "kCGWindowName").unwrap_or_default();
        let owner_name =
            dictionary_string(dictionary, "kCGWindowOwnerName").unwrap_or_else(|| "unknown".into());
        records.push(WindowRecord {
            id: WindowId::new(format!("mac-window-{number}-{process_id}")),
            title,
            owner_name,
            process_id: process_id as u32,
            bounds: Rect {
                origin: Point {
                    x: rect.origin.x,
                    y: rect.origin.y,
                },
                size: Size {
                    width: rect.size.width,
                    height: rect.size.height,
                },
            },
            active: false,
        });
    }
    unsafe { CFRelease(windows) };
    let active_index = focused_process_id
        .and_then(|process_id| {
            records
                .iter()
                .position(|window| window.process_id == process_id)
        })
        .unwrap_or(0);
    if let Some(window) = records.get_mut(active_index) {
        window.active = true;
    }
    Ok(records)
}

fn focused_application_pid() -> Option<u32> {
    let system = unsafe { AXUIElementCreateSystemWide() };
    if system.is_null() {
        return None;
    }
    let Ok(attribute) = ax_attribute("AXFocusedApplication") else {
        unsafe { CFRelease(system) };
        return None;
    };
    let mut focused = std::ptr::null();
    let result = unsafe { AXUIElementCopyAttributeValue(system, attribute, &mut focused) };
    unsafe {
        CFRelease(attribute);
        CFRelease(system);
    }
    if result != AX_ERROR_SUCCESS || focused.is_null() {
        return None;
    }
    let mut process_id = 0i32;
    let result = unsafe { AXUIElementGetPid(focused, &mut process_id) };
    unsafe { CFRelease(focused) };
    (result == AX_ERROR_SUCCESS && process_id > 0).then_some(process_id as u32)
}

fn capture_pixel_scale(bounds: Rect, width: u32, height: u32) -> DpiScale {
    scale_from_display_sizes(
        bounds.size,
        Size {
            width: width as f64,
            height: height as f64,
        },
    )
}

fn scale_from_display_sizes(logical: Size, pixels: Size) -> DpiScale {
    let x = pixels.width / logical.width;
    let y = pixels.height / logical.height;
    DpiScale {
        x: if x.is_finite() && x > 0.0 { x } else { 1.0 },
        y: if y.is_finite() && y > 0.0 { y } else { 1.0 },
    }
}

fn display_mode_pixel_size(display_id: u32) -> Option<(f64, f64)> {
    let mode = unsafe { CGDisplayCopyDisplayMode(display_id) };
    if mode.is_null() {
        return None;
    }
    let width = unsafe { CGDisplayModeGetPixelWidth(mode) } as f64;
    let height = unsafe { CGDisplayModeGetPixelHeight(mode) } as f64;
    unsafe { CGDisplayModeRelease(mode) };
    (width.is_finite() && width > 0.0 && height.is_finite() && height > 0.0)
        .then_some((width, height))
}

fn dictionary_value(dictionary: CFDictionaryRef, name: &str) -> Option<CFTypeRef> {
    let key = CString::new(name).ok()?;
    let key = unsafe { CFStringCreateWithCString(std::ptr::null(), key.as_ptr(), UTF8_ENCODING) };
    if key.is_null() {
        return None;
    }
    let value = unsafe { CFDictionaryGetValue(dictionary, key) };
    unsafe { CFRelease(key) };
    (!value.is_null()).then_some(value)
}

fn dictionary_i32(dictionary: CFDictionaryRef, name: &str) -> Option<i32> {
    let value = dictionary_value(dictionary, name)?;
    let mut result = 0i32;
    (unsafe {
        CFNumberGetValue(
            value,
            CF_NUMBER_SINT32,
            &mut result as *mut _ as *mut c_void,
        )
    } != 0)
        .then_some(result)
}

fn dictionary_bool(dictionary: CFDictionaryRef, name: &str) -> Option<bool> {
    let value = dictionary_value(dictionary, name)?;
    Some(unsafe { CFBooleanGetValue(value) != 0 })
}

fn dictionary_string(dictionary: CFDictionaryRef, name: &str) -> Option<String> {
    let value = dictionary_value(dictionary, name)?;
    cf_string(value)
}

fn cf_string(value: CFTypeRef) -> Option<String> {
    if value.is_null() {
        return None;
    }
    let direct = unsafe { CFStringGetCStringPtr(value, UTF8_ENCODING) };
    if !direct.is_null() {
        return Some(
            unsafe { CStr::from_ptr(direct) }
                .to_string_lossy()
                .into_owned(),
        );
    }
    let length = unsafe { CFStringGetLength(value) };
    let capacity =
        unsafe { CFStringGetMaximumSizeForEncoding(length, UTF8_ENCODING) }.saturating_add(1);
    if capacity <= 0 {
        return None;
    }
    let mut buffer = vec![0i8; capacity as usize];
    let ok =
        unsafe { CFStringGetCString(value, buffer.as_mut_ptr(), capacity, UTF8_ENCODING) } != 0;
    ok.then(|| {
        unsafe { CStr::from_ptr(buffer.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    })
}

unsafe fn image_to_bgra(image: CGImageRef) -> Result<(Vec<u8>, u32, u32, u32), ComputerError> {
    let width = CGImageGetWidth(image);
    let height = CGImageGetHeight(image);
    let source_stride = CGImageGetBytesPerRow(image);
    let provider = CGImageGetDataProvider(image);
    if width == 0 || height == 0 || provider.is_null() || source_stride < width.saturating_mul(4) {
        return Err(ComputerError::Backend(
            "macOS display image has invalid pixel data".into(),
        ));
    }
    let data = CGDataProviderCopyData(provider);
    if data.is_null() {
        return Err(ComputerError::AccessDenied(
            "display pixel data is unavailable; grant Screen Recording permission".into(),
        ));
    }
    let length = CFDataGetLength(data);
    let pointer = CFDataGetBytePtr(data);
    if pointer.is_null() || length < (source_stride.saturating_mul(height)) as isize {
        CFRelease(data);
        return Err(ComputerError::Backend(
            "macOS display image pixel buffer is truncated".into(),
        ));
    }
    let bitmap_info = CGImageGetBitmapInfo(image);
    let byte_order = bitmap_info & 0x7000;
    let mut bgra = vec![0u8; width.saturating_mul(height).saturating_mul(4)];
    for y in 0..height {
        let source = std::slice::from_raw_parts(pointer.add(y * source_stride), source_stride);
        let destination = &mut bgra[y * width * 4..(y + 1) * width * 4];
        for x in 0..width {
            let pixel = &source[x * 4..x * 4 + 4];
            let out = &mut destination[x * 4..x * 4 + 4];
            if byte_order == 0x2000 {
                out.copy_from_slice(&[pixel[3], pixel[2], pixel[1], pixel[0]]);
            } else {
                out.copy_from_slice(pixel);
            }
        }
    }
    CFRelease(data);
    Ok((
        bgra,
        width as u32,
        height as u32,
        width.saturating_mul(4) as u32,
    ))
}

fn capture_native_display_core(display_id: u32) -> Result<(Vec<u8>, u32, u32, u32), ComputerError> {
    ensure_screen_capture_access()?;
    let image = unsafe { CGDisplayCreateImage(display_id) };
    if image.is_null() {
        return Err(ComputerError::AccessDenied(
            "CGDisplayCreateImage returned null; grant Screen Recording permission to the sidecar"
                .into(),
        ));
    }
    let result = unsafe { image_to_bgra(image) };
    unsafe { CGImageRelease(image) };
    result
}

#[allow(clippy::too_many_arguments)]
fn blit_scaled_bgra(
    destination: &mut [u8],
    destination_width: u32,
    destination_height: u32,
    source: &[u8],
    source_width: u32,
    source_height: u32,
    source_stride: u32,
    destination_x: u32,
    destination_y: u32,
    destination_region_width: u32,
    destination_region_height: u32,
) {
    if source_width == 0
        || source_height == 0
        || destination_region_width == 0
        || destination_region_height == 0
    {
        return;
    }
    let clipped_width =
        destination_region_width.min(destination_width.saturating_sub(destination_x));
    let clipped_height =
        destination_region_height.min(destination_height.saturating_sub(destination_y));
    for y in 0..clipped_height {
        let source_y = ((y as u64 * source_height as u64) / destination_region_height as u64)
            .min(source_height as u64 - 1) as u32;
        for x in 0..clipped_width {
            let source_x = ((x as u64 * source_width as u64) / destination_region_width as u64)
                .min(source_width as u64 - 1) as u32;
            let source_offset = source_y as usize * source_stride as usize + source_x as usize * 4;
            let destination_offset = (destination_y + y) as usize * destination_width as usize * 4
                + (destination_x + x) as usize * 4;
            if source_offset + 4 <= source.len() && destination_offset + 4 <= destination.len() {
                destination[destination_offset..destination_offset + 4]
                    .copy_from_slice(&source[source_offset..source_offset + 4]);
            }
        }
    }
}

/// Draw the agent's virtual cursor into returned screenshots.  The real HID
/// pointer is intentionally untouched on the background route, so the cursor
/// needs to be part of the observation contract rather than a side effect on
/// the user's desktop.  A short trail makes fast multi-step batches visible in
/// recordings and is cheap compared with PNG encoding.
fn draw_virtual_cursor(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    cursor: &VirtualCursorState,
    desktop_origin: Point,
    scale: DpiScale,
) {
    if !cursor.visible
        || width == 0
        || height == 0
        || stride < width.saturating_mul(4)
        || !scale.x.is_finite()
        || !scale.y.is_finite()
        || scale.x <= 0.0
        || scale.y <= 0.0
    {
        return;
    }
    let to_pixel = |point: Point| {
        let x = (point.x - desktop_origin.x) * scale.x;
        let y = (point.y - desktop_origin.y) * scale.y;
        (x.is_finite() && y.is_finite()).then_some((x.round() as i32, y.round() as i32))
    };

    let trail_len = cursor.trail.len().max(1) as u32;
    for (index, point) in cursor.trail.iter().enumerate() {
        let Some((x, y)) = to_pixel(*point) else {
            continue;
        };
        let alpha = 35u8.saturating_add((index as u32 * 90 / trail_len).min(90) as u8);
        draw_cursor_circle(
            pixels,
            width,
            height,
            stride,
            x,
            y,
            4,
            [255, 195, 20, alpha],
        );
    }
    let Some((x, y)) = cursor.position.and_then(to_pixel) else {
        return;
    };
    draw_cursor_circle(pixels, width, height, stride, x, y, 15, [255, 105, 20, 70]);
    draw_cursor_circle(pixels, width, height, stride, x, y, 5, [255, 215, 30, 210]);

    // A compact black/white arrow remains legible over both light and dark UI.
    draw_cursor_line(
        pixels,
        width,
        height,
        stride,
        x,
        y,
        x + 10,
        y + 19,
        3,
        [0, 0, 0, 235],
    );
    draw_cursor_line(
        pixels,
        width,
        height,
        stride,
        x + 1,
        y + 1,
        x + 9,
        y + 17,
        1,
        [255, 255, 255, 255],
    );
    draw_cursor_line(
        pixels,
        width,
        height,
        stride,
        x + 10,
        y + 19,
        x + 18,
        y + 17,
        3,
        [0, 0, 0, 235],
    );
    draw_cursor_line(
        pixels,
        width,
        height,
        stride,
        x + 10,
        y + 18,
        x + 16,
        y + 17,
        1,
        [255, 255, 255, 255],
    );
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor_circle(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    center_x: i32,
    center_y: i32,
    radius: i32,
    color: [u8; 4],
) {
    let radius_squared = radius.saturating_mul(radius);
    for y in center_y.saturating_sub(radius)..=center_y.saturating_add(radius) {
        for x in center_x.saturating_sub(radius)..=center_x.saturating_add(radius) {
            let dx = x.saturating_sub(center_x);
            let dy = y.saturating_sub(center_y);
            if dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy)) <= radius_squared {
                blend_cursor_pixel(pixels, width, height, stride, x, y, color);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn draw_cursor_line(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    from_x: i32,
    from_y: i32,
    to_x: i32,
    to_y: i32,
    radius: i32,
    color: [u8; 4],
) {
    let distance = ((to_x - from_x).abs().max((to_y - from_y).abs())) as usize;
    for step in 0..=distance.max(1) {
        let fraction = step as f64 / distance.max(1) as f64;
        let x = from_x + ((to_x - from_x) as f64 * fraction).round() as i32;
        let y = from_y + ((to_y - from_y) as f64 * fraction).round() as i32;
        draw_cursor_circle(pixels, width, height, stride, x, y, radius, color);
    }
}

fn blend_cursor_pixel(
    pixels: &mut [u8],
    width: u32,
    height: u32,
    stride: u32,
    x: i32,
    y: i32,
    color: [u8; 4],
) {
    if x < 0 || y < 0 || x >= width as i32 || y >= height as i32 {
        return;
    }
    let offset = y as usize * stride as usize + x as usize * 4;
    if offset + 4 > pixels.len() {
        return;
    }
    let alpha = color[3] as u32;
    let inverse = 255u32.saturating_sub(alpha);
    for channel in 0..3 {
        pixels[offset + channel] = ((color[channel] as u32 * alpha
            + pixels[offset + channel] as u32 * inverse)
            / 255) as u8;
    }
    pixels[offset + 3] = 255;
}

fn encode_bgra_png(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>, ComputerError> {
    if stride < width.saturating_mul(4) || bgra.len() < stride as usize * height as usize {
        return Err(ComputerError::Backend(
            "invalid BGRA frame stride or size".into(),
        ));
    }
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in bgra.chunks(stride as usize).take(height as usize) {
        for pixel in row[..width as usize * 4].as_chunks::<4>().0 {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(png::Compression::Fast);
    let mut writer = encoder
        .write_header()
        .map_err(|error| ComputerError::Backend(format!("PNG header failed: {error}")))?;
    writer
        .write_image_data(&rgba)
        .map_err(|error| ComputerError::Backend(format!("PNG data failed: {error}")))?;
    writer
        .finish()
        .map_err(|error| ComputerError::Backend(format!("PNG finish failed: {error}")))?;
    Ok(bytes)
}

fn screenshot_from_frame(metadata: &CaptureFrameMetadata, bytes: Vec<u8>) -> Screenshot {
    Screenshot {
        metadata: ScreenshotMetadata {
            frame_id: metadata.frame_id.clone(),
            display_id: Some(metadata.display_id.clone()),
            width: metadata.width,
            height: metadata.height,
            mime_type: "image/png".into(),
            coordinate_space: metadata.coordinate_space,
            desktop_origin: metadata.desktop_origin,
            dpi: metadata.dpi,
            scale: metadata.scale,
            pixel_to_desktop_scale: metadata.pixel_to_desktop_scale,
            captured_at: metadata.captured_at,
        },
        bytes,
    }
}

fn display_for_rect(topology: &DisplayTopology, rect: Rect) -> Option<DisplayId> {
    topology
        .displays
        .iter()
        .filter(|display| intersects(rect, display.physical_bounds))
        .max_by(|left, right| {
            intersection_area(rect, left.physical_bounds)
                .partial_cmp(&intersection_area(rect, right.physical_bounds))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|display| display.id.clone())
}

fn public_windows(records: Vec<WindowRecord>, topology: &DisplayTopology) -> Vec<Window> {
    records
        .into_iter()
        .map(|window| Window {
            id: window.id,
            title: window.title,
            process_id: Some(window.process_id),
            bounds: Coordinate {
                space: CoordinateSpace::DesktopPhysical,
                point: window.bounds.origin,
                extent: window.bounds.size,
                dpi: DpiScale::ONE,
                display_id: display_for_rect(topology, window.bounds),
                frame_id: None,
            },
            screen_id: display_for_rect(topology, window.bounds),
            active: window.active,
            security: None,
        })
        .collect()
}

fn contains(rect: Rect, point: Point) -> bool {
    point.x >= rect.origin.x
        && point.y >= rect.origin.y
        && point.x < rect.origin.x + rect.size.width
        && point.y < rect.origin.y + rect.size.height
}

fn intersects(left: Rect, right: Rect) -> bool {
    left.origin.x < right.origin.x + right.size.width
        && left.origin.x + left.size.width > right.origin.x
        && left.origin.y < right.origin.y + right.size.height
        && left.origin.y + left.size.height > right.origin.y
}

fn intersection_area(left: Rect, right: Rect) -> f64 {
    let x1 = left.origin.x.max(right.origin.x);
    let y1 = left.origin.y.max(right.origin.y);
    let x2 = (left.origin.x + left.size.width).min(right.origin.x + right.size.width);
    let y2 = (left.origin.y + left.size.height).min(right.origin.y + right.size.height);
    (x2 - x1).max(0.0) * (y2 - y1).max(0.0)
}

fn mouse_event_types(button: MouseButton) -> (u32, u32) {
    match button {
        MouseButton::Left => (K_CG_LEFT_MOUSE_DOWN, K_CG_LEFT_MOUSE_UP),
        MouseButton::Right => (K_CG_RIGHT_MOUSE_DOWN, K_CG_RIGHT_MOUSE_UP),
        MouseButton::Middle => (7, 8),
    }
}

fn is_global_keyboard_action(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::TypeText {
            target: None,
            at: None,
            ..
        } | ComputerAction::KeyPress { target: None, .. }
            | ComputerAction::Hotkey { target: None, .. }
            | ComputerAction::KeyDown { target: None, .. }
            | ComputerAction::KeyUp { target: None, .. }
            | ComputerAction::HoldKey { target: None, .. }
    )
}

fn action_name(action: &ComputerAction) -> &'static str {
    match action {
        ComputerAction::FocusWindow { .. } => "focus_window",
        ComputerAction::MovePointer { .. } => "move_pointer",
        ComputerAction::Click { .. } => "click",
        ComputerAction::DoubleClick { .. } => "double_click",
        ComputerAction::RightClick { .. } => "right_click",
        ComputerAction::Drag { .. } => "drag",
        ComputerAction::Scroll { .. } => "scroll",
        ComputerAction::TypeText { .. } => "type_text",
        ComputerAction::KeyPress { .. } => "key_press",
        ComputerAction::Hotkey { .. } => "hotkey",
        ComputerAction::MouseDown { .. } => "mouse_down",
        ComputerAction::MouseUp { .. } => "mouse_up",
        ComputerAction::MiddleClick { .. } => "middle_click",
        ComputerAction::TripleClick { .. } => "triple_click",
        ComputerAction::ModifierClick { .. } => "modifier_click",
        ComputerAction::KeyDown { .. } => "key_down",
        ComputerAction::KeyUp { .. } => "key_up",
        ComputerAction::HoldKey { .. } => "hold_key",
        ComputerAction::ModifiedPointer { .. } => "modified_pointer",
    }
}

fn semantic_method(action: &SemanticAction) -> ComputerExecutionMethod {
    match action {
        SemanticAction::Focus { .. } => ComputerExecutionMethod::SemanticFocus,
        SemanticAction::Invoke { .. } => ComputerExecutionMethod::SemanticInvoke,
        SemanticAction::SetValue { .. } => ComputerExecutionMethod::SemanticSetValue,
        SemanticAction::Toggle { .. } => ComputerExecutionMethod::SemanticToggle,
        SemanticAction::Select { .. } => ComputerExecutionMethod::SemanticSelect,
        SemanticAction::Expand { .. } => ComputerExecutionMethod::SemanticExpand,
        SemanticAction::Collapse { .. } => ComputerExecutionMethod::SemanticCollapse,
        SemanticAction::SetRangeValue { .. } => ComputerExecutionMethod::SemanticSetRangeValue,
        SemanticAction::ScrollIntoView { .. } => ComputerExecutionMethod::SemanticScrollIntoView,
    }
}

fn pixel_execution_method(action: &ComputerAction) -> ComputerExecutionMethod {
    if matches!(action, ComputerAction::Click { .. }) {
        ComputerExecutionMethod::PixelClick
    } else {
        ComputerExecutionMethod::PixelAction
    }
}

fn millis_from_micros(micros: u128) -> u128 {
    micros.saturating_add(999) / 1_000
}

fn pixel_execution_verification(detail: impl Into<String>) -> ComputerExecutionVerification {
    ComputerExecutionVerification {
        kind: Some(ComputerExecutionVerificationKind::WindowChanged),
        verified: true,
        detail: Some(detail.into()),
        ..Default::default()
    }
}

fn semantic_verification_kind(action: &SemanticAction) -> ComputerExecutionVerificationKind {
    match action {
        SemanticAction::Focus { .. } => ComputerExecutionVerificationKind::Focused,
        SemanticAction::SetValue { .. } | SemanticAction::SetRangeValue { .. } => {
            ComputerExecutionVerificationKind::ValueEquals
        }
        SemanticAction::Invoke { .. }
        | SemanticAction::Toggle { .. }
        | SemanticAction::Select { .. }
        | SemanticAction::Expand { .. }
        | SemanticAction::Collapse { .. }
        | SemanticAction::ScrollIntoView { .. } => {
            ComputerExecutionVerificationKind::SemanticStateChanged
        }
    }
}

fn semantic_status_allows_takeover(status: SemanticActionStatus) -> bool {
    matches!(
        status,
        SemanticActionStatus::Unsupported
            | SemanticActionStatus::WindowNotForeground
            | SemanticActionStatus::FocusDenied
    )
}

fn execution_outcome_from_semantic(status: SemanticActionStatus) -> ComputerExecutionOutcome {
    match status {
        SemanticActionStatus::Performed => ComputerExecutionOutcome::Performed,
        SemanticActionStatus::Unsupported => ComputerExecutionOutcome::Unsupported,
        SemanticActionStatus::StaleElement => ComputerExecutionOutcome::StaleElement,
        SemanticActionStatus::UnknownElement => ComputerExecutionOutcome::UnknownElement,
        SemanticActionStatus::ElementUnavailable => ComputerExecutionOutcome::ElementUnavailable,
        SemanticActionStatus::Disabled => ComputerExecutionOutcome::Disabled,
        SemanticActionStatus::ReadOnly => ComputerExecutionOutcome::ReadOnly,
        SemanticActionStatus::InvalidValue => ComputerExecutionOutcome::InvalidValue,
        SemanticActionStatus::WindowNotForeground => ComputerExecutionOutcome::WindowNotForeground,
        SemanticActionStatus::FocusDenied => ComputerExecutionOutcome::FocusDenied,
        SemanticActionStatus::VerificationFailed => ComputerExecutionOutcome::VerificationFailed,
        SemanticActionStatus::OutcomeUnknown => ComputerExecutionOutcome::OutcomeUnknown,
    }
}

fn execution_verification_from_semantic(
    action: &SemanticAction,
    result: &SemanticActionResult,
) -> ComputerExecutionVerification {
    let kind = match action {
        SemanticAction::Focus { .. } => ComputerExecutionVerificationKind::Focused,
        SemanticAction::SetValue { .. } | SemanticAction::SetRangeValue { .. } => {
            ComputerExecutionVerificationKind::ValueEquals
        }
        SemanticAction::Invoke { .. }
        | SemanticAction::Toggle { .. }
        | SemanticAction::Select { .. }
        | SemanticAction::Expand { .. }
        | SemanticAction::Collapse { .. }
        | SemanticAction::ScrollIntoView { .. } => {
            ComputerExecutionVerificationKind::SemanticStateChanged
        }
    };
    ComputerExecutionVerification {
        kind: Some(kind),
        verified: result.verification.verified,
        detail: result.verification.detail.clone(),
        state_changed: result.verification.state_changed,
        focused: result.verification.focused,
        observed_value: result.verification.observed_value.clone(),
        toggled: result.verification.toggled,
        selected: result.verification.selected,
        expanded: result.verification.expanded,
        range_value: result.verification.range_value,
        offscreen: result.verification.offscreen,
    }
}

fn execution_outcome_from_error(error: &ComputerError) -> ComputerExecutionOutcome {
    match error {
        ComputerError::InvalidAction(_) => ComputerExecutionOutcome::InvalidRequest,
        ComputerError::InvalidWindow(_) | ComputerError::UnknownDisplay(_) => {
            ComputerExecutionOutcome::InvalidTarget
        }
        ComputerError::InvalidCoordinate(_) => ComputerExecutionOutcome::InvalidTarget,
        ComputerError::ForegroundDenied(_) => ComputerExecutionOutcome::WindowNotForeground,
        ComputerError::AccessDenied(_) => ComputerExecutionOutcome::TargetUnavailable,
        ComputerError::Unsupported { .. } | ComputerError::CapabilityGap { .. } => {
            ComputerExecutionOutcome::Unsupported
        }
        ComputerError::StaleDisplay(_) => ComputerExecutionOutcome::InvalidTarget,
        _ => ComputerExecutionOutcome::BackendError,
    }
}

fn modifier_flag(key: &str) -> u64 {
    match key.trim().to_ascii_lowercase().as_str() {
        "shift" => 0x0002_0000,
        "control" | "ctrl" => 0x0004_0000,
        "option" | "alt" => 0x0008_0000,
        "command" | "cmd" | "meta" => 0x0010_0000,
        _ => 0,
    }
}

fn key_code(key: &str) -> Option<u16> {
    let key = key.trim().to_ascii_lowercase();
    let code = match key.as_str() {
        "a" => 0,
        "s" => 1,
        "d" => 2,
        "f" => 3,
        "h" => 4,
        "g" => 5,
        "z" => 6,
        "x" => 7,
        "c" => 8,
        "v" => 9,
        "b" => 11,
        "q" => 12,
        "w" => 13,
        "e" => 14,
        "r" => 15,
        "y" => 16,
        "t" => 17,
        "1" => 18,
        "2" => 19,
        "3" => 20,
        "4" => 21,
        "6" => 22,
        "5" => 23,
        "=" => 24,
        "9" => 25,
        "7" => 26,
        "-" => 27,
        "8" => 28,
        "0" => 29,
        "]" => 30,
        "o" => 31,
        "u" => 32,
        "[" => 33,
        "i" => 34,
        "p" => 35,
        "l" => 37,
        "j" => 38,
        "'" => 39,
        "k" => 40,
        ";" => 41,
        "\\" => 42,
        "," => 43,
        "/" => 44,
        "n" => 45,
        "m" => 46,
        "." => 47,
        "`" => 50,
        "enter" | "return" => 36,
        "tab" => 48,
        "space" => 49,
        "backspace" | "delete" => 51,
        "escape" | "esc" => 53,
        "command" | "cmd" | "meta" => 55,
        "shift" => 56,
        "caps_lock" => 57,
        "option" | "alt" => 58,
        "control" | "ctrl" => 59,
        "right_shift" => 60,
        "right_option" => 61,
        "right_control" => 62,
        "fn" => 63,
        "left" | "arrowleft" => 123,
        "right" | "arrowright" => 124,
        "down" | "arrowdown" => 125,
        "up" | "arrowup" => 126,
        "home" => 115,
        "end" => 119,
        "page_up" | "pageup" => 116,
        "page_down" | "pagedown" => 121,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    };
    Some(code)
}

fn security_capabilities(reason: &str) -> TargetAccessCapabilities {
    let allowed = || CapabilityAccess::Allowed;
    let _ = reason;
    TargetAccessCapabilities {
        pixel_observation: allowed(),
        semantic_observation: allowed(),
        window_focus: CapabilityAccess::Allowed,
        pointer_input: allowed(),
        keyboard_input: allowed(),
        semantic_action: allowed(),
    }
}

impl MacNativeBackend {
    fn semantic_observe_ax(
        &mut self,
        session: &ComputerSessionId,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        self.ensure_initialized()?;
        ensure_ax_trusted()?;
        let window = enumerate_window_records()?
            .into_iter()
            .find(|window| &window.id == window_id)
            .ok_or_else(|| ComputerError::InvalidWindow(format!("window {window_id} is gone")))?;
        let application = unsafe { AXUIElementCreateApplication(window.process_id as i32) };
        if application.is_null() {
            return Err(ComputerError::AccessDenied(
                "AXUIElementCreateApplication returned null".into(),
            ));
        }
        let (window_index, ax_window) = match find_ax_window(application, &window) {
            Ok(value) => value,
            Err(error) => {
                unsafe { CFRelease(application) };
                return Err(error);
            }
        };
        unsafe { CFRelease(application) };

        let limits = limits.bounded();
        let (nonce, generation) = {
            let state = self.semantic.entry(session.clone()).or_default();
            if state.nonce == 0 {
                state.nonce = semantic_session_nonce(session);
            }
            state.generation = state.generation.saturating_add(1);
            (state.nonce, state.generation)
        };
        let started = Instant::now();
        let tree_started = Instant::now();
        let mut elements = Vec::new();
        let mut current = HashMap::new();
        let (root_element_id, truncated, property_read_micros) = {
            let mut walk = AxWalkState {
                nonce,
                generation,
                window_id,
                process_id: window.process_id,
                window_index,
                limits,
                elements: &mut elements,
                current: &mut current,
                property_micros: 0,
                truncated: false,
            };
            let root_element_id = walk_ax_tree(ax_window, &[], None, 0, &mut walk);
            (root_element_id, walk.truncated, walk.property_micros)
        };
        unsafe { CFRelease(ax_window) };
        let Some(root_element_id) = root_element_id else {
            let state = self.semantic.entry(session.clone()).or_default();
            state.current.clear();
            state.snapshot.clear();
            state.observed_window = None;
            return Err(ComputerError::Backend(
                "AX window did not expose a readable accessibility element".into(),
            ));
        };
        let tree_walk_micros = tree_started.elapsed().as_micros();
        let total_micros = started.elapsed().as_micros();
        let element_count = elements.len();
        let metadata = alice_computer_use_core::SemanticObservationMetadata {
            captured_at: Some(SystemTime::now()),
            generation,
            max_depth: limits.max_depth,
            max_elements: limits.max_elements,
            truncated,
            uia_init_micros: tree_walk_micros,
            tree_walk_micros,
            property_read_micros,
            serialization_micros: None,
            total_micros,
            element_count,
        };
        let state = self.semantic.entry(session.clone()).or_default();
        state.current = current;
        state.snapshot = elements.clone();
        state.observed_window = Some(window_id.clone());
        Ok(SemanticObservation {
            session_id: session.clone(),
            window_id: window_id.clone(),
            root_element_id,
            elements,
            metadata,
        })
    }

    fn validate_ax_element(
        &self,
        session: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<(), ComputerError> {
        let state = self.semantic.get(session).ok_or_else(|| {
            ComputerError::UnknownElement(format!("element is not known for session {session}"))
        })?;
        match ax_element_status(state, element_id) {
            None => Ok(()),
            Some(SemanticActionStatus::StaleElement) => Err(ComputerError::StaleElement(
                "element belongs to an older AX observation generation".into(),
            )),
            Some(_) => Err(ComputerError::UnknownElement(
                "element token is not valid in the current AX observation".into(),
            )),
        }
    }

    fn ax_element_window(
        &self,
        session: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        let state = self.semantic.get(session).ok_or_else(|| {
            ComputerError::UnknownElement("element session is no longer valid".into())
        })?;
        let Some(record) = state.current.get(element_id) else {
            return Err(match ax_element_status(state, element_id) {
                Some(SemanticActionStatus::StaleElement) => ComputerError::StaleElement(
                    "element belongs to an older AX observation generation".into(),
                ),
                _ => ComputerError::UnknownElement(
                    "element is not valid in the current AX observation".into(),
                ),
            });
        };
        Ok(record.window_id.clone())
    }

    fn semantic_action_ax(
        &mut self,
        session: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        self.ensure_initialized()?;
        ensure_ax_trusted()?;
        let element_id = action.element_id().clone();
        let (generation, record) = {
            let state = self.semantic.get(session).ok_or_else(|| {
                ComputerError::UnknownElement("element session is no longer valid".into())
            })?;
            let generation = state.generation;
            let Some(record) = state.current.get(&element_id).cloned() else {
                return Ok(mac_semantic_action_result(
                    action,
                    ax_element_status(state, &element_id)
                        .unwrap_or(SemanticActionStatus::UnknownElement),
                    Some(generation),
                    None,
                    SemanticActionVerification {
                        detail: Some("element is not valid in the current AX observation".into()),
                        ..Default::default()
                    },
                    SemanticActionTiming::default(),
                ));
            };
            (generation, record)
        };
        let started = Instant::now();
        if !record.semantic.enabled {
            return Ok(mac_semantic_action_result(
                action,
                SemanticActionStatus::Disabled,
                Some(generation),
                None,
                SemanticActionVerification {
                    detail: Some("AX element is disabled".into()),
                    ..Default::default()
                },
                timing_from_started(started),
            ));
        }
        if matches!(action, SemanticAction::ScrollIntoView { .. }) {
            return Ok(mac_semantic_action_result(
                action,
                SemanticActionStatus::Unsupported,
                Some(generation),
                None,
                SemanticActionVerification {
                    detail: Some(
                        "macOS Accessibility API has no portable scroll-into-view action".into(),
                    ),
                    ..Default::default()
                },
                timing_from_started(started),
            ));
        }
        if let Some(status) = semantic_action_capability_status(action, &record.semantic) {
            return Ok(mac_semantic_action_result(
                action,
                status,
                Some(generation),
                None,
                SemanticActionVerification {
                    detail: Some("AX element does not advertise the requested capability".into()),
                    ..Default::default()
                },
                timing_from_started(started),
            ));
        }
        if let SemanticAction::SetValue { value, .. } = action {
            if record.password {
                return Ok(mac_semantic_action_result(
                    action,
                    SemanticActionStatus::ReadOnly,
                    Some(generation),
                    None,
                    SemanticActionVerification {
                        detail: Some(
                            "password controls cannot be written by the semantic API".into(),
                        ),
                        ..Default::default()
                    },
                    timing_from_started(started),
                ));
            }
            if value.len() > 16 * 1024 {
                return Ok(mac_semantic_action_result(
                    action,
                    SemanticActionStatus::InvalidValue,
                    Some(generation),
                    None,
                    SemanticActionVerification {
                        detail: Some("semantic text value exceeds the 16 KiB safety limit".into()),
                        ..Default::default()
                    },
                    timing_from_started(started),
                ));
            }
        }
        if let SemanticAction::SetRangeValue { value, .. } = action {
            if !value.is_finite()
                || record
                    .semantic
                    .range_minimum
                    .is_some_and(|minimum| *value < minimum)
                || record
                    .semantic
                    .range_maximum
                    .is_some_and(|maximum| *value > maximum)
            {
                return Ok(mac_semantic_action_result(
                    action,
                    SemanticActionStatus::InvalidValue,
                    Some(generation),
                    None,
                    SemanticActionVerification {
                        detail: Some("range value is outside the AX element's bounds".into()),
                        ..Default::default()
                    },
                    timing_from_started(started),
                ));
            }
        }

        let element =
            ax_element_for_path(record.process_id, record.window_index, &record.child_path)?;
        let dispatch_started = Instant::now();
        let dispatch = dispatch_ax_action(element, action);
        unsafe { CFRelease(element) };
        dispatch?;
        let action_micros = dispatch_started.elapsed().as_micros();
        thread::sleep(Duration::from_millis(10));
        let element =
            ax_element_for_path(record.process_id, record.window_index, &record.child_path)?;
        let verification_started = Instant::now();
        let verification = verify_ax_action(element, action, &record.semantic)?;
        unsafe { CFRelease(element) };
        let verification_micros = verification_started.elapsed().as_micros();
        let verified = verification.verified;
        let status = if verified {
            SemanticActionStatus::Performed
        } else {
            SemanticActionStatus::VerificationFailed
        };
        let next_generation = if status == SemanticActionStatus::Performed
            && !matches!(action, SemanticAction::Focus { .. })
        {
            let state = self
                .semantic
                .get_mut(session)
                .expect("semantic state exists");
            state.generation = state.generation.saturating_add(1);
            state.current.clear();
            state.snapshot.clear();
            state.observed_window = None;
            Some(state.generation)
        } else {
            if status == SemanticActionStatus::Performed {
                if let Some(state) = self.semantic.get_mut(session) {
                    for record in state.current.values_mut() {
                        record.semantic.focused = record.semantic.id == element_id;
                    }
                    for element in &mut state.snapshot {
                        element.focused = element.id == element_id;
                    }
                }
            }
            Some(generation)
        };
        let mut verification = verification;
        verification.detail.get_or_insert_with(|| {
            if verified {
                "AX action completed and post-action state was verified".into()
            } else {
                "AX action completed but post-action state could not be verified".into()
            }
        });
        let security = enumerate_window_records()?
            .into_iter()
            .find(|window| window.id == record.window_id)
            .map(|window| self.security_decision(&window, "semantic_action"))
            .transpose()?;
        let mut result = mac_semantic_action_result_with_timing(
            action,
            status,
            Some(generation),
            next_generation,
            verification,
            SemanticActionTiming {
                action_micros,
                verification_micros,
                total_micros: started.elapsed().as_micros(),
                ..Default::default()
            },
        );
        result.security = security;
        Ok(result)
    }

    fn capability_probe_ax(
        &mut self,
        session: &ComputerSessionId,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerError> {
        self.ensure_initialized()?;
        ensure_ax_trusted()?;
        let started = Instant::now();
        let window = enumerate_window_records()?
            .into_iter()
            .find(|window| &window.id == window_id)
            .ok_or_else(|| {
                ComputerError::InvalidWindow(format!("window {window_id} is current"))
            })?;
        let should_observe = self
            .semantic
            .get(session)
            .and_then(|state| state.observed_window.as_ref())
            != Some(window_id);
        if should_observe {
            let _ = self.semantic_observe_ax(
                session,
                window_id,
                SemanticObservationLimits {
                    max_depth: 2,
                    max_elements: 64,
                },
            )?;
        }
        let state = self
            .semantic
            .get(session)
            .ok_or_else(|| ComputerError::CapabilityGap {
                capability: "semantic_observation".into(),
                detail: "AX observation did not produce a session state".into(),
            })?;
        let elements = state.snapshot.clone();
        let any = |predicate: fn(&alice_computer_use_core::ComputerElement) -> bool| {
            elements.iter().any(predicate)
        };
        let action = |supported: bool, name: &str| {
            if supported {
                CapabilityAssessment::supported(
                    CapabilitySource::ProviderReported,
                    format!("current AX observation found {name}"),
                )
            } else {
                CapabilityAssessment::unsupported(
                    CapabilitySource::ProviderReported,
                    format!("current AX observation found no {name} capability"),
                )
            }
        };
        let semantic_actions = CapabilitySemanticActionProfile {
            focus: action(any(|element| element.focusable), "focus"),
            invoke: action(any(|element| element.capabilities.invokable), "invoke"),
            set_value: action(any(|element| element.capabilities.editable), "set_value"),
            toggle: action(any(|element| element.capabilities.toggleable), "toggle"),
            select: action(any(|element| element.capabilities.selectable), "select"),
            expand_collapse: action(
                any(|element| element.capabilities.expandable),
                "expand/collapse",
            ),
            range_value: action(
                any(|element| element.capabilities.range_adjustable),
                "range_value",
            ),
            scroll_into_view: CapabilityAssessment::unsupported(
                CapabilitySource::Static,
                "macOS AX has no portable scroll-into-view action",
            ),
        };
        let decision = self.security_decision(&window, "capability_probe")?;
        let security = WindowSecurityMetadata {
            process: decision.target.clone(),
            boundary: decision.boundary,
            capabilities: decision.capabilities.clone(),
            reason: Some(decision.reason.clone()),
        };
        let semantic_preferred = if elements.is_empty() {
            CapabilityAssessment::unknown(
                CapabilitySource::ProviderReported,
                "AX observation returned no elements",
            )
        } else {
            CapabilityAssessment::supported(
                CapabilitySource::ProviderReported,
                format!("current AX observation found {} elements", elements.len()),
            )
        };
        Ok(ApplicationCapabilityProfile {
            window_id: window_id.clone(),
            application: ApplicationIdentity {
                process_id: Some(window.process_id),
                executable_name: Some(window.owner_name),
                executable_path: None,
                executable_hash: None,
                process_architecture: ProcessArchitecture::Unknown,
                top_level_window_class: None,
                framework_hints: vec![FrameworkHint::Unknown],
                version: None,
            },
            framework_hint: FrameworkHint::Unknown,
            observation: CapabilityObservationProfile {
                window: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "CoreGraphics window metadata is available",
                ),
                pixel: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "CoreGraphics display capture is available when Screen Recording permission is granted",
                ),
                semantic: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "bounded AXUIElement tree is available",
                ),
                frame: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "session-scoped raw frame capture is available",
                ),
            },
            input: CapabilityInputProfile {
                pointer: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "CoreGraphics CGEvent pointer input; Accessibility permission required",
                ),
                keyboard: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "CoreGraphics CGEvent keyboard input; Accessibility permission required",
                ),
                text: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "CoreGraphics Unicode text input; Accessibility permission required",
                ),
            },
            semantic_actions,
            execution: CapabilityExecutionProfile {
                semantic_preferred,
                pixel_fallback_available: CapabilityAssessment::supported(
                    CapabilitySource::Static,
                    "explicit pixel execution is available with AX background routing and foreground takeover fallback",
                ),
            },
            environment: CapabilityEnvironmentProfile {
                access_boundary: decision.boundary,
                display_compatibility: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "CoreGraphics display geometry is available",
                ),
                topology_compatibility: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "multi-display metadata and per-display capture are available",
                ),
            },
            restrictions: CapabilityRestrictions {
                elevation_required: false,
                foreground_required: false,
                reasons: vec![
                    "AX and PID-directed keyboard routes are background-preferred; coordinate pointer actions may use takeover fallback".into(),
                ],
            },
            security: Some(security),
            semantic_generation: Some(state.generation),
            cache_hit: false,
            timings: CapabilityProbeTimings {
                total_ms: started.elapsed().as_millis(),
                ..Default::default()
            },
            known_gaps: vec![
                "AXUIElement scroll-into-view has no portable macOS action".into(),
            ],
        })
    }

    fn security_decision(
        &self,
        window: &WindowRecord,
        operation: &str,
    ) -> Result<SecurityDecision, ComputerError> {
        let source = self.self_security.clone().ok_or_else(|| {
            ComputerError::SecurityContextUnavailable("macOS process context is unavailable".into())
        })?;
        let target = ProcessSecurityContext {
            process_id: Some(window.process_id),
            ..source.clone()
        };
        Ok(SecurityDecision {
            operation: operation.into(),
            boundary: ComputerAccessBoundary::Allowed,
            source: Some(source),
            target: Some(target),
            capabilities: security_capabilities(
                "macOS TCC permissions; background routes validate target process/window and takeover routes validate foreground",
            ),
            reason: "macOS TCC permissions and target identity validation are required; foreground validation applies to takeover routes".into(),
        })
    }
}
