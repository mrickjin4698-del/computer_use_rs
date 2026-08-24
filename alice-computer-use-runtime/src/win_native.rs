//! Windows-native observation, input, and semantic action backend.
//!
//! UI Automation/COM is confined to `uia.rs`; only the R2 semantic action
//! contract is exposed here.
//! This backend does not read the clipboard or launch processes.

#[path = "uia.rs"]
mod uia;

use super::{Capability, CapabilityState, ComputerBackend};
use alice_computer_use_core::{
    ActionStatus, ApplicationCapabilityProfile, ApplicationIdentity, CapabilityAccess,
    CapabilityAssessment, CapabilityEnvironmentProfile, CapabilityExecutionProfile,
    CapabilityInputProfile, CapabilityObservationProfile, CapabilityProbeTimings,
    CapabilityRestrictions, CapabilitySemanticActionProfile, CapabilitySource, CapabilityStatus,
    CaptureFrameMetadata, ComputerAccessBoundary, ComputerAction, ComputerActionResult,
    ComputerError, ComputerExecutionAttempt, ComputerExecutionIntent, ComputerExecutionMethod,
    ComputerExecutionOutcome, ComputerExecutionRequest, ComputerExecutionResult,
    ComputerExecutionStrategy, ComputerExecutionTiming, ComputerExecutionVerification,
    ComputerExecutionVerificationKind, ComputerFallbackPolicy, ComputerSessionId, Coordinate,
    CoordinateSpace, CoordinateTransform, DesktopKind, DesktopSecurityContext, DisplayId,
    DisplayInfo, DisplayTopology, DpiScale, ElementId, FrameEncoding, FrameEncodingResult, FrameId,
    FrameMetadataResult, FramePixelFormat, FrameState, FrameworkHint, IntegrityLevel, MouseButton,
    PixelTarget, Point, ProcessArchitecture, ProcessSecurityContext, Rect, Screen, ScreenId,
    Screenshot, ScreenshotMetadata, ScreenshotTarget, ScrollDirection, SecurityDecision,
    SemanticAction, SemanticActionResult, SemanticActionStatus, SemanticObservation,
    SemanticObservationLimits, Size, TargetAccessCapabilities, Window, WindowId,
    WindowSecurityMetadata,
};
use async_trait::async_trait;
use std::{
    collections::{HashMap, HashSet, VecDeque},
    mem::size_of,
    thread,
    time::{Duration, Instant, SystemTime},
};
use uia::UiaStore;
use windows::core::PWSTR;
use windows::Win32::{
    Foundation::{CloseHandle, GetLastError, BOOL, HANDLE, HWND, LPARAM, POINT, RECT},
    Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
        EnumDisplayMonitors, GetDC, GetDIBits, GetMonitorInfoW, ReleaseDC, SelectObject,
        BITMAPINFO, BITMAPINFOHEADER, BI_RGB, CAPTUREBLT, DIB_RGB_COLORS, HMONITOR, SRCCOPY,
    },
    Security::{
        GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenElevation,
        TokenIntegrityLevel, TokenIsAppContainer, TokenUIAccess, TOKEN_ELEVATION,
        TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
    },
    System::{
        StationsAndDesktops::{
            GetThreadDesktop, GetUserObjectInformationW, OpenInputDesktop, DESKTOP_ACCESS_FLAGS,
            DESKTOP_CONTROL_FLAGS, UOI_NAME,
        },
        Threading::{
            AttachThreadInput, GetCurrentProcessId, GetCurrentThreadId, OpenProcess,
            OpenProcessToken, QueryFullProcessImageNameW, PROCESS_NAME_FORMAT,
            PROCESS_QUERY_LIMITED_INFORMATION,
        },
    },
    UI::{
        HiDpi::{
            GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
            DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        },
        Input::KeyboardAndMouse::{
            MapVirtualKeyW, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT,
            KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE,
            KEYEVENTF_UNICODE, MAPVK_VK_TO_VSC, MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL,
            MOUSEEVENTF_MOVE, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, VIRTUAL_KEY,
        },
        WindowsAndMessaging::{
            BringWindowToTop, EnumWindows, GetAncestor, GetClassNameW, GetCursorPos,
            GetForegroundWindow, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
            GetWindowThreadProcessId, IsIconic, IsWindow, IsWindowVisible, SetForegroundWindow,
            SetWindowPos, ShowWindow, WindowFromPoint, GA_ROOT, HWND_TOP, MONITORINFOF_PRIMARY,
            SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW, SW_RESTORE,
        },
    },
};

/// Production WinNative observation/input implementation with bounded UIA
/// semantic observation and Focus/Invoke/SetValue actions.
///
/// Every keyboard operation is a single `SendInput` transaction and every target
/// action confirms the target HWND before sending input.
pub struct WinNativeBackend {
    initialized: bool,
    dpi: u32,
    uia: UiaStore,
    pressed: HashMap<ComputerSessionId, PressedState>,
    topology: Option<NativeTopology>,
    next_frame: u64,
    frames: HashMap<ComputerSessionId, FrameStore>,
    self_security: Option<ProcessSecurityContext>,
    capability_cache: HashMap<ComputerSessionId, CapabilityCache>,
}

const CAPABILITY_CACHE_MAX: usize = 32;
const VERIFICATION_RETRY_DELAYS_MS: &[u64] = &[25, 75, 150, 250];
// Stable process-lifetime attribution marker consumed by the host-owned
// low-level input monitor. It identifies Alice's own SendInput events; it is
// not treated as a security boundary.
const ALICE_COMPUTER_INPUT_MARKER: usize =
    alice_computer_use_core::ALICE_COMPUTER_INPUT_MARKER as usize;

fn configured_input_marker() -> usize {
    std::env::var("ALICE_COMPUTER_INPUT_MARKER")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value != 0)
        .unwrap_or(ALICE_COMPUTER_INPUT_MARKER)
}

struct CapabilityCacheEntry {
    process_id: Option<u32>,
    security_key: String,
    topology_generation: u64,
    semantic_generation: u64,
    profile: ApplicationCapabilityProfile,
}

#[derive(Default)]
struct CapabilityCache {
    entries: HashMap<WindowId, CapabilityCacheEntry>,
}

#[derive(Default)]
struct PressedState {
    mouse_buttons: HashSet<MouseButton>,
    keys: HashSet<String>,
}

const MAX_HOLD_KEY_MS: u32 = 5_000;

struct WindowEnumState {
    windows: Vec<Window>,
    foreground: HWND,
    topology: DisplayTopology,
}

#[derive(Clone, Copy)]
struct NativeMonitor {
    physical_bounds: Rect,
    work_area: Rect,
    dpi: DpiScale,
    primary: bool,
}

struct NativeTopology {
    topology: DisplayTopology,
    monitors: HashMap<DisplayId, NativeMonitor>,
}

const FRAME_STORE_MAX_FRAMES: usize = 8;
const FRAME_STORE_MAX_BYTES: usize = 128 * 1024 * 1024;
const FRAME_STORE_MAX_ENCODED_BYTES: usize = 32 * 1024 * 1024;
const FRAME_TOMBSTONE_LIMIT: usize = 64;

struct StoredFrame {
    metadata: CaptureFrameMetadata,
    pixels: Vec<u8>,
    encoded_png: Option<Vec<u8>>,
}

struct FrameStore {
    frames: HashMap<FrameId, StoredFrame>,
    order: VecDeque<FrameId>,
    tombstones: HashMap<FrameId, FrameState>,
    tombstone_order: VecDeque<FrameId>,
    total_bytes: usize,
    encoded_bytes: usize,
    max_frames: usize,
    max_bytes: usize,
    max_encoded_bytes: usize,
    encode_cache_hits: u64,
    encode_cache_misses: u64,
    capture_count: u64,
    total_capture_micros: u128,
    total_store_micros: u128,
}

impl Default for FrameStore {
    fn default() -> Self {
        Self {
            frames: HashMap::new(),
            order: VecDeque::new(),
            tombstones: HashMap::new(),
            tombstone_order: VecDeque::new(),
            total_bytes: 0,
            encoded_bytes: 0,
            max_frames: FRAME_STORE_MAX_FRAMES,
            max_bytes: FRAME_STORE_MAX_BYTES,
            max_encoded_bytes: FRAME_STORE_MAX_ENCODED_BYTES,
            encode_cache_hits: 0,
            encode_cache_misses: 0,
            capture_count: 0,
            total_capture_micros: 0,
            total_store_micros: 0,
        }
    }
}

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
        if bytes > self.max_bytes {
            return Err(ComputerError::Backend(format!(
                "capture frame is too large for bounded store: {bytes} > {}",
                self.max_bytes
            )));
        }
        while self.frames.len() >= self.max_frames
            || self.total_bytes.saturating_add(bytes) > self.max_bytes
        {
            self.evict_one();
        }
        let id = frame.metadata.frame_id.clone();
        self.total_bytes = self.total_bytes.saturating_add(bytes);
        self.order.push_back(id.clone());
        self.frames.insert(id, frame);
        Ok(())
    }

    fn state(&self, id: &FrameId, topology_generation: u64) -> FrameMetadataResult {
        if let Some(frame) = self.frames.get(id) {
            let mut metadata = frame.metadata.clone();
            let stale = metadata.topology_generation != topology_generation;
            metadata.stale_topology = stale;
            return FrameMetadataResult {
                metadata: Some(metadata),
                state: if stale {
                    FrameState::StaleTopology
                } else {
                    FrameState::Current
                },
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
        if let Some(frame) = self.frames.remove(id) {
            self.order.retain(|candidate| candidate != id);
            self.total_bytes = self.total_bytes.saturating_sub(frame.pixels.len());
            self.encoded_bytes = self
                .encoded_bytes
                .saturating_sub(frame.encoded_png.as_ref().map_or(0, Vec::len));
            self.remember(id.clone(), FrameState::Released);
            FrameMetadataResult {
                metadata: Some(frame.metadata),
                state: FrameState::Released,
            }
        } else {
            FrameMetadataResult {
                metadata: None,
                state: self
                    .tombstones
                    .get(id)
                    .copied()
                    .unwrap_or(FrameState::Unknown),
            }
        }
    }

    fn encode(
        &mut self,
        id: &FrameId,
        topology_generation: u64,
    ) -> Result<(Screenshot, bool), ComputerError> {
        let state = self.state(id, topology_generation);
        let metadata = state.metadata.ok_or_else(|| {
            ComputerError::Backend(format!("frame {id} is not readable ({:?})", state.state))
        })?;
        if let Some(bytes) = self
            .frames
            .get(id)
            .and_then(|frame| frame.encoded_png.clone())
        {
            self.encode_cache_hits = self.encode_cache_hits.saturating_add(1);
            return Ok((screenshot_from_frame(&metadata, bytes), true));
        }
        self.encode_cache_misses = self.encode_cache_misses.saturating_add(1);
        let frame = self.frames.get(id).ok_or_else(|| {
            ComputerError::Backend(format!("frame {id} disappeared from frame store"))
        })?;
        let bytes = encode_bgra_png(
            &frame.pixels,
            metadata.width,
            metadata.height,
            metadata.stride,
        )?;
        if bytes.len() <= self.max_encoded_bytes {
            self.encoded_bytes = self.encoded_bytes.saturating_add(bytes.len());
            if let Some(frame) = self.frames.get_mut(id) {
                frame.encoded_png = Some(bytes.clone());
            }
            while self.encoded_bytes > self.max_encoded_bytes {
                let Some(candidate) = self.order.front().cloned() else {
                    break;
                };
                if let Some(frame) = self.frames.get_mut(&candidate) {
                    if let Some(old) = frame.encoded_png.take() {
                        self.encoded_bytes = self.encoded_bytes.saturating_sub(old.len());
                        continue;
                    }
                }
                self.order.rotate_left(1);
            }
        }
        Ok((screenshot_from_frame(&metadata, bytes), false))
    }
}

struct MonitorEnumState {
    monitors: Vec<NativeMonitor>,
}

unsafe extern "system" fn enum_monitor_proc(
    monitor: HMONITOR,
    _dc: windows::Win32::Graphics::Gdi::HDC,
    _clip: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let state = &mut *(lparam.0 as *mut MonitorEnumState);
    let mut info = windows::Win32::Graphics::Gdi::MONITORINFO {
        cbSize: size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
        ..Default::default()
    };
    if !GetMonitorInfoW(monitor, &mut info).as_bool() {
        return BOOL(1);
    }
    let physical_bounds = core_rect(info.rcMonitor);
    let work_area = core_rect(info.rcWork);
    let center = POINT {
        x: (physical_bounds.origin.x + physical_bounds.size.width / 2.0).round() as i32,
        y: (physical_bounds.origin.y + physical_bounds.size.height / 2.0).round() as i32,
    };
    let window = WindowFromPoint(center);
    let dpi = if !window.0.is_null() {
        GetDpiForWindow(window).max(96)
    } else {
        GetDpiForSystem().max(96)
    } as f64
        / 96.0;
    let primary = info.dwFlags & MONITORINFOF_PRIMARY != 0;
    state.monitors.push(NativeMonitor {
        physical_bounds,
        work_area,
        dpi: DpiScale { x: dpi, y: dpi },
        primary,
    });
    BOOL(1)
}

struct PixelAttemptResult {
    outcome: ComputerExecutionOutcome,
    verification: ComputerExecutionVerification,
    generation_after: Option<u64>,
    verification_ms: u128,
    detail: Option<String>,
}

impl PixelAttemptResult {
    fn error(outcome: ComputerExecutionOutcome, detail: String) -> Self {
        Self {
            outcome,
            verification: ComputerExecutionVerification {
                kind: Some(ComputerExecutionVerificationKind::WindowChanged),
                detail: Some(detail.clone()),
                ..Default::default()
            },
            generation_after: None,
            verification_ms: 0,
            detail: Some(detail),
        }
    }
}

unsafe extern "system" fn enum_window_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let state = &mut *(lparam.0 as *mut WindowEnumState);
    if !IsWindowVisible(hwnd).as_bool() {
        return BOOL(1);
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return BOOL(1);
    }
    let title_len = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut title_buffer = vec![0u16; title_len.saturating_add(1).max(1)];
    let title_chars = GetWindowTextW(hwnd, &mut title_buffer).max(0) as usize;
    let title = String::from_utf16_lossy(&title_buffer[..title_chars.min(title_buffer.len())]);
    let mut process_id = 0u32;
    let _ = GetWindowThreadProcessId(hwnd, Some(&mut process_id));
    let bounds = core_rect(rect);
    let display_id = state
        .topology
        .displays
        .iter()
        .filter(|display| intersects(bounds, display.physical_bounds))
        .max_by(|left, right| {
            intersection_area(bounds, left.physical_bounds)
                .partial_cmp(&intersection_area(bounds, right.physical_bounds))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|display| display.id.clone());
    state.windows.push(Window {
        id: WindowId::new((hwnd.0 as isize).to_string()),
        title,
        process_id: (process_id != 0).then_some(process_id),
        bounds: Coordinate {
            space: CoordinateSpace::DesktopPhysical,
            point: bounds.origin,
            extent: bounds.size,
            dpi: DpiScale::ONE,
            display_id: None,
            frame_id: None,
        },
        screen_id: display_id,
        active: hwnd == state.foreground,
        security: None,
    });
    BOOL(1)
}

fn core_rect(rect: RECT) -> Rect {
    Rect {
        origin: Point {
            x: rect.left as f64,
            y: rect.top as f64,
        },
        size: Size {
            width: (rect.right - rect.left).max(0) as f64,
            height: (rect.bottom - rect.top).max(0) as f64,
        },
    }
}

fn logical_local_rect(rect: Rect, display_origin: Point, scale: DpiScale) -> Rect {
    Rect {
        origin: Point {
            x: (rect.origin.x - display_origin.x) / scale.x.max(1.0),
            y: (rect.origin.y - display_origin.y) / scale.y.max(1.0),
        },
        size: Size {
            width: rect.size.width / scale.x.max(1.0),
            height: rect.size.height / scale.y.max(1.0),
        },
    }
}

fn intersects(left: Rect, right: Rect) -> bool {
    let left_right = left.origin.x + left.size.width;
    let left_bottom = left.origin.y + left.size.height;
    let right_right = right.origin.x + right.size.width;
    let right_bottom = right.origin.y + right.size.height;
    left.origin.x < right_right
        && left_right > right.origin.x
        && left.origin.y < right_bottom
        && left_bottom > right.origin.y
}

fn intersection_area(left: Rect, right: Rect) -> f64 {
    let x1 = left.origin.x.max(right.origin.x);
    let y1 = left.origin.y.max(right.origin.y);
    let x2 = (left.origin.x + left.size.width).min(right.origin.x + right.size.width);
    let y2 = (left.origin.y + left.size.height).min(right.origin.y + right.size.height);
    (x2 - x1).max(0.0) * (y2 - y1).max(0.0)
}

fn mouse_button_flags(
    button: MouseButton,
) -> (
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    match button {
        MouseButton::Left => (MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP),
        MouseButton::Right => (MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP),
        MouseButton::Middle => (MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePngMode {
    Current,
    Fast,
}

#[derive(Clone, Debug)]
pub struct NativeCaptureProfile {
    pub screenshot: Screenshot,
    pub capture_micros: u128,
    pub pixel_readback_micros: u128,
    pub pixel_conversion_micros: u128,
    pub png_encode_micros: u128,
    pub total_micros: u128,
    pub png_bytes: usize,
}

struct GdiCaptureResult {
    bgra: Vec<u8>,
    width: u32,
    height: u32,
    stride: u32,
    capture_micros: u128,
    pixel_readback_micros: u128,
}

fn capture_gdi_bgra(
    source_x: i32,
    source_y: i32,
    width: i32,
    height: i32,
) -> Result<GdiCaptureResult, ComputerError> {
    let started = Instant::now();
    let screen_dc = unsafe { GetDC(None) };
    if screen_dc.0.is_null() {
        return Err(ComputerError::Backend(format!(
            "GetDC(NULL) failed (win32_error={})",
            unsafe { GetLastError().0 }
        )));
    }
    let memory_dc = unsafe { CreateCompatibleDC(screen_dc) };
    if memory_dc.0.is_null() {
        unsafe { ReleaseDC(None, screen_dc) };
        return Err(ComputerError::Backend("CreateCompatibleDC failed".into()));
    }
    let bitmap = unsafe { CreateCompatibleBitmap(screen_dc, width, height) };
    if bitmap.0.is_null() {
        unsafe {
            let _ = DeleteDC(memory_dc);
            ReleaseDC(None, screen_dc);
        }
        return Err(ComputerError::Backend(
            "CreateCompatibleBitmap failed".into(),
        ));
    }

    let result = unsafe {
        let result = (|| {
            let previous = SelectObject(memory_dc, bitmap);
            if previous.0.is_null() {
                return Err(ComputerError::Backend("SelectObject(bitmap) failed".into()));
            }
            BitBlt(
                memory_dc,
                0,
                0,
                width,
                height,
                screen_dc,
                source_x,
                source_y,
                SRCCOPY | CAPTUREBLT,
            )
            .map_err(|error| ComputerError::Backend(format!("BitBlt failed: {error}")))?;
            let _ = SelectObject(memory_dc, previous);
            let capture_micros = started.elapsed().as_micros();

            let mut bitmap_info = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bgra = vec![0u8; width as usize * height as usize * 4];
            let readback_started = Instant::now();
            let copied = GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                Some(bgra.as_mut_ptr() as *mut core::ffi::c_void),
                &mut bitmap_info,
                DIB_RGB_COLORS,
            );
            if copied != height {
                return Err(ComputerError::Backend(format!(
                    "GetDIBits copied {} of {} rows (win32_error={})",
                    copied,
                    height,
                    GetLastError().0
                )));
            }
            let pixel_readback_micros = readback_started.elapsed().as_micros();
            Ok(GdiCaptureResult {
                bgra,
                width: width as u32,
                height: height as u32,
                stride: width as u32 * 4,
                capture_micros,
                pixel_readback_micros,
            })
        })();
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(memory_dc);
        ReleaseDC(None, screen_dc);
        result
    }?;
    Ok(result)
}

fn encode_bgra_png_with_mode(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: u32,
    png_mode: NativePngMode,
) -> Result<(Vec<u8>, u128, u128), ComputerError> {
    if stride < width.saturating_mul(4) || bgra.len() < stride as usize * height as usize {
        return Err(ComputerError::Backend(
            "invalid BGRA frame stride or size".into(),
        ));
    }
    let conversion_started = Instant::now();
    let mut rgba = Vec::with_capacity(width as usize * height as usize * 4);
    for row in bgra.chunks(stride as usize).take(height as usize) {
        for pixel in row[..width as usize * 4].chunks_exact(4) {
            rgba.extend_from_slice(&[pixel[2], pixel[1], pixel[0], pixel[3]]);
        }
    }
    let pixel_conversion_micros = conversion_started.elapsed().as_micros();
    let encode_started = Instant::now();
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    encoder.set_compression(match png_mode {
        NativePngMode::Current => png::Compression::default(),
        NativePngMode::Fast => png::Compression::Fast,
    });
    let mut writer = encoder
        .write_header()
        .map_err(|error| ComputerError::Backend(format!("PNG header failed: {error}")))?;
    writer
        .write_image_data(&rgba)
        .map_err(|error| ComputerError::Backend(format!("PNG data failed: {error}")))?;
    writer
        .finish()
        .map_err(|error| ComputerError::Backend(format!("PNG finish failed: {error}")))?;
    Ok((
        bytes,
        pixel_conversion_micros,
        encode_started.elapsed().as_micros(),
    ))
}

fn encode_bgra_png(
    bgra: &[u8],
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>, ComputerError> {
    encode_bgra_png_with_mode(bgra, width, height, stride, NativePngMode::Fast)
        .map(|(bytes, _, _)| bytes)
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
            captured_at: metadata.captured_at,
        },
        bytes,
    }
}

/// Native primary-display facts kept outside the backend-neutral Core contract.
/// The public coordinates contained in observations are DesktopPhysical; this
/// diagnostic view additionally carries the native primary display geometry.
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

impl WinNativeBackend {
    pub fn new() -> Self {
        Self {
            initialized: false,
            dpi: 96,
            uia: UiaStore::default(),
            pressed: HashMap::new(),
            topology: None,
            next_frame: 0,
            frames: HashMap::new(),
            self_security: None,
            capability_cache: HashMap::new(),
        }
    }

    pub fn frame_store_stats(&self, session: &ComputerSessionId) -> NativeFrameStoreStats {
        self.frames
            .get(session)
            .map(|store| NativeFrameStoreStats {
                frame_count: store.frames.len(),
                total_bytes: store.total_bytes,
                max_frames: store.max_frames,
                max_bytes: store.max_bytes,
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

    fn ensure_initialized(&self) -> Result<(), ComputerError> {
        if self.initialized {
            Ok(())
        } else {
            Err(ComputerError::NotInitialized)
        }
    }

    fn self_security_context(&self) -> Result<ProcessSecurityContext, ComputerError> {
        self.self_security.clone().ok_or_else(|| {
            ComputerError::SecurityContextUnavailable(
                "WinNative process security context was not initialized".into(),
            )
        })
    }

    fn inspect_target_security(
        &self,
        window_id: &WindowId,
    ) -> Result<(HWND, ProcessSecurityContext), ComputerError> {
        let hwnd = Self::hwnd(window_id).map_err(|error| match error {
            ComputerError::InvalidWindow(detail) => ComputerError::TargetUnavailable(detail),
            other => other,
        })?;
        let (pid, context) = inspect_hwnd_security(hwnd)?;
        let mut after_pid = 0u32;
        unsafe {
            if !IsWindow(hwnd).as_bool()
                || GetWindowThreadProcessId(hwnd, Some(&mut after_pid)) == 0
                || after_pid != pid
            {
                return Err(ComputerError::TargetUnavailable(format!(
                    "window {} changed or exited during security inspection",
                    window_id
                )));
            }
        }
        Ok((hwnd, context))
    }

    fn desktop_security_context_now(&self) -> Result<DesktopSecurityContext, ComputerError> {
        let (current_name, input_name) = self.desktop_identity_now()?;
        let same = current_name.eq_ignore_ascii_case(&input_name);
        Ok(DesktopSecurityContext {
            // Desktop names are case-insensitive Win32 object names.  The
            // handles themselves are intentionally not compared: separate
            // opens of the same desktop can have different handle values.
            desktop_kind: if same {
                DesktopKind::InteractiveUserDesktop
            } else {
                DesktopKind::ProtectedOrSecureDesktop
            },
            interactive: same,
            protected: !same,
        })
    }

    pub fn desktop_identity(&self) -> Result<(String, String), ComputerError> {
        self.desktop_identity_now()
    }

    fn desktop_identity_now(&self) -> Result<(String, String), ComputerError> {
        unsafe {
            let current = GetThreadDesktop(GetCurrentThreadId()).map_err(|error| {
                ComputerError::SecurityContextUnavailable(format!(
                    "GetThreadDesktop failed: {error}"
                ))
            })?;
            let input = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(1))
                .map_err(|error| {
                    ComputerError::SecurityContextUnavailable(format!(
                        "OpenInputDesktop failed: {error}"
                    ))
                })?;
            let names = (
                desktop_name(HANDLE(current.0)),
                desktop_name(HANDLE(input.0)),
            );
            let _ = windows::Win32::System::StationsAndDesktops::CloseDesktop(input);
            Ok((names.0?, names.1?))
        }
    }

    fn security_decision_for_hwnd(
        &self,
        hwnd: HWND,
        operation: &str,
    ) -> Result<SecurityDecision, ComputerError> {
        let source = self.self_security_context()?;
        let (_, target) = inspect_hwnd_security(hwnd)?;
        let desktop = self.desktop_security_context_now()?;
        let boundary = if desktop.protected {
            ComputerAccessBoundary::ProtectedDesktop
        } else if target.integrity_level == IntegrityLevel::Unknown
            || source.integrity_level == IntegrityLevel::Unknown
        {
            ComputerAccessBoundary::Unknown
        } else if integrity_rank(target.integrity_level) > integrity_rank(source.integrity_level) {
            ComputerAccessBoundary::ElevationRequired
        } else {
            ComputerAccessBoundary::Allowed
        };
        let reason = boundary_reason(boundary, &source, &target, &desktop);
        Ok(SecurityDecision {
            operation: operation.into(),
            boundary,
            source: Some(source),
            target: Some(target),
            capabilities: capabilities_for_boundary(boundary, &reason),
            reason,
        })
    }

    fn admit_security(decision: &SecurityDecision) -> Result<(), ComputerError> {
        match decision.boundary {
            ComputerAccessBoundary::Allowed => Ok(()),
            ComputerAccessBoundary::IntegrityMismatch => {
                Err(ComputerError::IntegrityMismatch(decision.reason.clone()))
            }
            ComputerAccessBoundary::ElevationRequired => {
                Err(ComputerError::ElevationRequired(decision.reason.clone()))
            }
            ComputerAccessBoundary::ProtectedDesktop => {
                Err(ComputerError::ProtectedDesktop(decision.reason.clone()))
            }
            ComputerAccessBoundary::TargetUnavailable => {
                Err(ComputerError::TargetUnavailable(decision.reason.clone()))
            }
            ComputerAccessBoundary::Unknown => Err(ComputerError::SecurityContextUnavailable(
                decision.reason.clone(),
            )),
        }
    }

    fn security_decision_for_window_id(
        &self,
        window_id: &WindowId,
        operation: &str,
    ) -> Result<SecurityDecision, ComputerError> {
        let (hwnd, _) = self.inspect_target_security(window_id)?;
        self.security_decision_for_hwnd(hwnd, operation)
    }

    fn preflight_action(
        &self,
        session: &ComputerSessionId,
        action: &ComputerAction,
    ) -> Result<SecurityDecision, ComputerError> {
        let operation = action_name(action);
        let hwnd = match action {
            ComputerAction::FocusWindow { window_id } => {
                let decision = self.security_decision_for_window_id(window_id, operation)?;
                Self::admit_security(&decision)?;
                return Ok(decision);
            }
            ComputerAction::TypeText {
                at: Some(at),
                target,
                ..
            } => {
                let (x, y, _) = self.coordinate(session, at)?;
                let point_window = unsafe { GetAncestor(WindowFromPoint(POINT { x, y }), GA_ROOT) };
                if let Some(window_id) = target {
                    let target_window = self.inspect_target_security(window_id)?.0;
                    if point_window != target_window {
                        return Err(ComputerError::InvalidCoordinate(format!(
                            "type_text screenshot point resolves outside target window {window_id}"
                        )));
                    }
                }
                point_window
            }
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
            | ComputerAction::HoldKey { target, .. } => match target {
                Some(window_id) => self.inspect_target_security(window_id)?.0,
                None => unsafe { GetForegroundWindow() },
            },
            ComputerAction::MovePointer { to }
            | ComputerAction::Click { at: to }
            | ComputerAction::DoubleClick { at: to }
            | ComputerAction::RightClick { at: to }
            | ComputerAction::Scroll { at: to, .. } => {
                let (x, y, _) = self.coordinate(session, to)?;
                unsafe { WindowFromPoint(POINT { x, y }) }
            }
            ComputerAction::Drag { from, .. } => {
                let (x, y, _) = self.coordinate(session, from)?;
                unsafe { WindowFromPoint(POINT { x, y }) }
            }
        };
        if hwnd.0.is_null() {
            return Err(ComputerError::TargetUnavailable(format!(
                "{} has no live target window",
                operation
            )));
        }
        if !self.foreground_matches(hwnd) {
            return Err(ComputerError::ForegroundDenied(format!(
                "{} target is not the current foreground window; input rejected",
                operation
            )));
        }
        let decision = self.security_decision_for_hwnd(hwnd, operation)?;
        Self::admit_security(&decision)?;
        Ok(decision)
    }

    fn foreground_matches(&self, target: HWND) -> bool {
        unsafe {
            let foreground = GetForegroundWindow();
            if foreground.0.is_null() {
                return false;
            }
            let target_root = GetAncestor(target, GA_ROOT);
            let foreground_root = GetAncestor(foreground, GA_ROOT);
            !target_root.0.is_null()
                && !foreground_root.0.is_null()
                && target_root == foreground_root
        }
    }

    fn hwnd(window_id: &WindowId) -> Result<HWND, ComputerError> {
        let raw = window_id.as_str().parse::<isize>().map_err(|_| {
            ComputerError::InvalidWindow(format!(
                "WinNative window id is not a numeric HWND: {}",
                window_id.as_str()
            ))
        })?;
        let hwnd = HWND(raw as *mut _);
        if hwnd.0.is_null() {
            return Err(ComputerError::InvalidWindow("null HWND".into()));
        }
        let valid = unsafe { IsWindow(hwnd).as_bool() };
        if !valid {
            return Err(ComputerError::InvalidWindow(format!(
                "HWND {} is no longer a live window (win32_error={})",
                window_id,
                unsafe { GetLastError().0 }
            )));
        }
        Ok(hwnd)
    }

    fn focus(&self, window_id: &WindowId) -> Result<HWND, ComputerError> {
        let hwnd = Self::hwnd(window_id)?;
        unsafe {
            let before = GetForegroundWindow();
            let was_minimized = IsIconic(hwnd).as_bool();
            if was_minimized {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
            let _ = BringWindowToTop(hwnd);
            let _ = SetWindowPos(
                hwnd,
                HWND_TOP,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let foreground = GetForegroundWindow();
            let foreground_thread = if foreground.0.is_null() {
                0
            } else {
                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(foreground, None)
            };
            let current_thread = GetCurrentThreadId();
            let attached = foreground_thread != 0
                && foreground_thread != current_thread
                && AttachThreadInput(current_thread, foreground_thread, true).as_bool();
            let set_result = SetForegroundWindow(hwnd).as_bool();
            let set_error = if set_result { 0 } else { GetLastError().0 };
            let after = GetForegroundWindow();
            if attached {
                let _ = AttachThreadInput(current_thread, foreground_thread, false);
            }
            if after != hwnd {
                let detail = format!(
                    "target={} before={:?} after={:?} set_foreground_result={} set_error={} minimized={} thread_attached={}",
                    window_id,
                    before,
                    after,
                    set_result,
                    set_error,
                    was_minimized,
                    attached,
                );
                return if !set_result {
                    Err(ComputerError::ForegroundDenied(detail))
                } else {
                    Err(ComputerError::FocusNotAcquired(detail))
                };
            }
        }
        Ok(hwnd)
    }

    fn foreground_target(&self, target: Option<&WindowId>) -> Result<HWND, ComputerError> {
        match target {
            Some(window_id) => {
                let hwnd = Self::hwnd(window_id)?;
                if unsafe { GetForegroundWindow() } == hwnd {
                    Ok(hwnd)
                } else {
                    Err(ComputerError::ForegroundDenied(format!(
                        "input target {} is not the current foreground window; input rejected",
                        window_id
                    )))
                }
            }
            None => {
                let hwnd = unsafe { GetForegroundWindow() };
                if hwnd.0.is_null() {
                    Err(ComputerError::ForegroundDenied(
                        "no current foreground window; input rejected".into(),
                    ))
                } else {
                    Ok(hwnd)
                }
            }
        }
    }

    fn coordinate(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
    ) -> Result<(i32, i32, String), ComputerError> {
        if coordinate.extent.width <= 0.0
            || coordinate.extent.height <= 0.0
            || coordinate.dpi.x <= 0.0
            || coordinate.dpi.y <= 0.0
        {
            return Err(ComputerError::InvalidCoordinate(
                "coordinate extent and DPI must be positive".into(),
            ));
        }
        let native = self.topology()?;
        let transform = CoordinateTransform::new(&native.topology);
        let (point, detail) = match coordinate.space {
            CoordinateSpace::DesktopPhysical => (
                Point {
                    x: coordinate.point.x.round(),
                    y: coordinate.point.y.round(),
                },
                format!(
                    "space=desktop_physical; point=({},{}); extent={}x{}",
                    coordinate.point.x,
                    coordinate.point.y,
                    coordinate.extent.width,
                    coordinate.extent.height
                ),
            ),
            CoordinateSpace::DisplayPhysical => {
                let display_id = coordinate.display_id.as_ref().ok_or_else(|| {
                    ComputerError::InvalidCoordinate(
                        "display_physical coordinate requires display_id".into(),
                    )
                })?;
                self.validate_display_id(display_id)?;
                let point = transform.display_physical_to_desktop(display_id, coordinate.point)?;
                (
                    Point {
                        x: point.x.round(),
                        y: point.y.round(),
                    },
                    format!(
                        "space=display_physical; display_id={display_id}; local=({},{}); desktop=({}, {})",
                        coordinate.point.x, coordinate.point.y, point.x, point.y
                    ),
                )
            }
            CoordinateSpace::DisplayLogical => {
                let display_id = coordinate.display_id.as_ref().ok_or_else(|| {
                    ComputerError::InvalidCoordinate(
                        "display_logical coordinate requires display_id".into(),
                    )
                })?;
                self.validate_display_id(display_id)?;
                let physical =
                    transform.display_logical_to_physical(display_id, coordinate.point)?;
                let point = transform.display_physical_to_desktop(display_id, physical)?;
                (
                    point,
                    format!(
                        "space=display_logical; display_id={display_id}; logical=({},{}); desktop=({}, {})",
                        coordinate.point.x, coordinate.point.y, point.x, point.y
                    ),
                )
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
                let state = store.state(frame_id, native.topology.topology_generation);
                if state.state != FrameState::Current {
                    return Err(ComputerError::StaleDisplay(format!(
                        "screenshot frame {frame_id} is not current ({:?})",
                        state.state
                    )));
                }
                let metadata = state.metadata.as_ref().ok_or_else(|| {
                    ComputerError::StaleDisplay(format!(
                        "screenshot frame {frame_id} is unavailable"
                    ))
                })?;
                let screenshot_metadata = ScreenshotMetadata {
                    frame_id: metadata.frame_id.clone(),
                    display_id: Some(metadata.display_id.clone()),
                    width: metadata.width,
                    height: metadata.height,
                    mime_type: "image/png".into(),
                    coordinate_space: metadata.coordinate_space,
                    desktop_origin: metadata.desktop_origin,
                    dpi: metadata.dpi,
                    scale: metadata.scale,
                    captured_at: metadata.captured_at,
                };
                let point =
                    transform.screenshot_to_desktop(&screenshot_metadata, coordinate.point)?;
                (
                    Point {
                        x: point.x.round(),
                        y: point.y.round(),
                    },
                    format!(
                        "space=screenshot_pixel; frame_id={frame_id}; pixel=({},{}); desktop=({}, {})",
                        coordinate.point.x, coordinate.point.y, point.x, point.y
                    ),
                )
            }
            CoordinateSpace::Screen => {
                // R0-R5 compatibility only: Screen is a primary-display
                // local physical frame. No source extent rescaling is done.
                let primary_id = native
                    .topology
                    .primary_display_id
                    .as_ref()
                    .ok_or_else(|| ComputerError::Backend("no primary display".into()))?;
                self.validate_display_id(coordinate.display_id.as_ref().unwrap_or(primary_id))?;
                let point = transform.display_physical_to_desktop(
                    coordinate.display_id.as_ref().unwrap_or(primary_id),
                    coordinate.point,
                )?;
                (
                    Point {
                        x: point.x.round(),
                        y: point.y.round(),
                    },
                    format!(
                        "space=screen_legacy; primary_display_id={primary_id}; local=({},{}); desktop=({}, {})",
                        coordinate.point.x, coordinate.point.y, point.x, point.y
                    ),
                )
            }
            CoordinateSpace::Window => {
                return Err(ComputerError::CapabilityGap {
                    capability: "window_coordinate_actions".into(),
                    detail: "Window coordinates require explicit target resolution".into(),
                })
            }
        };
        let x = point.x as i32;
        let y = point.y as i32;
        Ok((x, y, format!("{detail}; physical_target=({}, {})", x, y)))
    }

    fn send_text(&self, text: &str) -> Result<String, ComputerError> {
        let mut events = Vec::with_capacity(text.encode_utf16().count() * 2);
        let mut previous_cr = false;
        for ch in text.chars() {
            match ch {
                '\n' if previous_cr => previous_cr = false,
                '\n' | '\r' => {
                    events.push(key_input(vk("enter")?, false));
                    events.push(key_input(vk("enter")?, true));
                    previous_cr = ch == '\r';
                }
                _ => {
                    previous_cr = false;
                    let mut units = [0u16; 2];
                    for unit in ch.encode_utf16(&mut units) {
                        events.push(unicode_input(*unit, false));
                        events.push(unicode_input(*unit, true));
                    }
                }
            }
        }
        if events.is_empty() {
            return Ok("unicode_utf16_events=0".into());
        }
        unsafe { send_input(&events)? };
        Ok(format!(
            "unicode_utf16_events={}; utf16_units={}",
            events.len(),
            text.encode_utf16().count()
        ))
    }

    fn send_key(&self, key: &str) -> Result<String, ComputerError> {
        let key = vk(key)?;
        let events = [key_input(key, false), key_input(key, true)];
        unsafe { send_input(&events)? };
        Ok(format!(
            "virtual_key=0x{:02x}; physical_scancode=true",
            key.0
        ))
    }

    fn send_hotkey(&self, keys: &[String]) -> Result<String, ComputerError> {
        if keys.len() < 2 {
            return Err(ComputerError::InvalidAction(
                "WinNative hotkey requires at least one modifier and one key".into(),
            ));
        }
        let mut modifiers = Vec::new();
        let mut primary = None;
        for key in keys {
            if let Some(modifier) = modifier_vk(key) {
                modifiers.push(modifier);
            } else if primary.replace(vk(key)?).is_some() {
                return Err(ComputerError::InvalidAction(
                    "WinNative hotkey supports one non-modifier key".into(),
                ));
            }
        }
        let Some(primary) = primary else {
            return Err(ComputerError::InvalidAction(
                "WinNative hotkey has no non-modifier key".into(),
            ));
        };
        if modifiers.is_empty() {
            return Err(ComputerError::InvalidAction(
                "WinNative hotkey requires a modifier".into(),
            ));
        }
        let mut events = Vec::with_capacity(modifiers.len() * 2 + 2);
        for modifier in &modifiers {
            events.push(virtual_key_input(*modifier, false));
        }
        events.push(virtual_key_input(primary, false));
        events.push(virtual_key_input(primary, true));
        for modifier in modifiers.iter().rev() {
            events.push(virtual_key_input(*modifier, true));
        }
        unsafe {
            if let Err(error) = send_input(&events) {
                release_modifiers(&modifiers);
                return Err(ComputerError::Backend(format!(
                    "SendInput hotkey failed; modifier release attempted: {error}"
                )));
            }
        }
        Ok(format!(
            "modifiers={} primary_vk=0x{:02x}; modifier_release_order=reverse",
            modifiers.len(),
            primary.0
        ))
    }

    fn refresh_topology(&mut self) -> Result<(), ComputerError> {
        self.ensure_initialized()?;
        let mut state = MonitorEnumState {
            monitors: Vec::new(),
        };
        let enum_result = unsafe {
            EnumDisplayMonitors(
                None,
                None,
                Some(enum_monitor_proc),
                LPARAM((&mut state as *mut MonitorEnumState) as isize),
            )
        };
        if !enum_result.as_bool() || state.monitors.is_empty() {
            return Err(ComputerError::Backend(format!(
                "EnumDisplayMonitors failed or returned no monitors (win32_error={})",
                unsafe { GetLastError().0 }
            )));
        }
        state.monitors.sort_by(|left, right| {
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
        let previous = self.topology.take();
        let same_topology = previous.as_ref().is_some_and(|old| {
            old.monitors.len() == state.monitors.len()
                && state.monitors.iter().all(|monitor| {
                    old.monitors.values().any(|previous_monitor| {
                        previous_monitor.physical_bounds == monitor.physical_bounds
                            && previous_monitor.work_area == monitor.work_area
                            && previous_monitor.dpi == monitor.dpi
                            && previous_monitor.primary == monitor.primary
                    })
                })
        });
        let generation = previous
            .as_ref()
            .map(|old| {
                if same_topology {
                    old.topology.topology_generation
                } else {
                    old.topology.topology_generation.saturating_add(1)
                }
            })
            .unwrap_or(1);
        let mut displays = Vec::with_capacity(state.monitors.len());
        let mut native = HashMap::new();
        let mut used_display_ids = HashSet::new();
        for (index, monitor) in state.monitors.into_iter().enumerate() {
            let id = if same_topology {
                previous
                    .as_ref()
                    .and_then(|old| {
                        old.monitors.iter().find_map(|(id, previous_monitor)| {
                            (previous_monitor.physical_bounds == monitor.physical_bounds
                                && previous_monitor.work_area == monitor.work_area
                                && previous_monitor.dpi == monitor.dpi
                                && previous_monitor.primary == monitor.primary
                                && !used_display_ids.contains(id))
                            .then(|| id.clone())
                        })
                    })
                    .unwrap_or_else(|| DisplayId::new(format!("display-{generation}-{index}")))
            } else {
                DisplayId::new(format!("display-{generation}-{index}"))
            };
            used_display_ids.insert(id.clone());
            let logical_size = Size {
                width: monitor.physical_bounds.size.width / monitor.dpi.x.max(1.0),
                height: monitor.physical_bounds.size.height / monitor.dpi.y.max(1.0),
            };
            displays.push(DisplayInfo {
                id: id.clone(),
                name: Some(format!("display-{index}")),
                physical_bounds: monitor.physical_bounds,
                work_area: monitor.work_area,
                physical_size: monitor.physical_bounds.size,
                logical_size,
                dpi: DpiScale {
                    x: monitor.dpi.x * 96.0,
                    y: monitor.dpi.y * 96.0,
                },
                scale: monitor.dpi,
                primary: monitor.primary,
            });
            native.insert(id, monitor);
        }
        self.topology = Some(NativeTopology {
            topology: DisplayTopology::from_displays(displays, generation),
            monitors: native,
        });
        Ok(())
    }

    fn topology(&self) -> Result<&NativeTopology, ComputerError> {
        self.topology.as_ref().ok_or(ComputerError::NotInitialized)
    }

    fn validate_display_id(&self, display_id: &DisplayId) -> Result<(), ComputerError> {
        let topology = self.topology()?;
        if topology
            .topology
            .displays
            .iter()
            .any(|display| display.id == *display_id)
        {
            return Ok(());
        }
        let stale = display_id
            .as_str()
            .strip_prefix("display-")
            .and_then(|value| value.split('-').next())
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|generation| generation < topology.topology.topology_generation);
        if stale {
            Err(ComputerError::StaleDisplay(format!(
                "display {display_id} belongs to an older topology generation (current={})",
                topology.topology.topology_generation
            )))
        } else {
            Err(ComputerError::UnknownDisplay(display_id.to_string()))
        }
    }

    /// Return primary-monitor geometry in physical and diagnostic logical
    /// units. The topology is enumerated separately and carries all displays.
    pub fn primary_display_info(&self) -> Result<NativeDisplayInfo, ComputerError> {
        self.ensure_initialized()?;
        let native = self.topology()?;
        let primary_id =
            native.topology.primary_display_id.clone().ok_or_else(|| {
                ComputerError::Backend("no primary display was enumerated".into())
            })?;
        let display = native
            .topology
            .display(&primary_id)
            .map_err(ComputerError::from)?;
        let _monitor = native
            .monitors
            .get(&primary_id)
            .ok_or_else(|| ComputerError::UnknownDisplay(primary_id.to_string()))?;
        Ok(NativeDisplayInfo {
            display_id: primary_id,
            logical_monitor: logical_local_rect(
                display.physical_bounds,
                display.physical_bounds.origin,
                display.scale,
            ),
            logical_work_area: logical_local_rect(
                display.work_area,
                display.physical_bounds.origin,
                display.scale,
            ),
            monitor: display.physical_bounds,
            work_area: display.work_area,
            dpi: display.scale,
            system_dpi: (display.dpi.x.round() as u32).max(96),
            scale: display.scale,
            virtual_desktop_bounds: native.topology.virtual_desktop_bounds,
            topology: native.topology.clone(),
        })
    }

    fn enumerate_window_records(
        &self,
        display: &NativeDisplayInfo,
    ) -> Result<Vec<Window>, ComputerError> {
        let mut state = WindowEnumState {
            windows: Vec::new(),
            foreground: unsafe { GetForegroundWindow() },
            topology: display.topology.clone(),
        };
        unsafe {
            EnumWindows(
                Some(enum_window_proc),
                LPARAM((&mut state as *mut WindowEnumState) as isize),
            )
            .map_err(|error| ComputerError::Backend(format!("EnumWindows failed: {error}")))?;
        }
        Ok(state
            .windows
            .into_iter()
            .map(|mut window| {
                let security = window
                    .id
                    .as_str()
                    .parse::<isize>()
                    .ok()
                    .map(|raw| self.window_security_metadata(HWND(raw as *mut _)));
                window.security = Some(security.unwrap_or_else(|| WindowSecurityMetadata {
                    process: None,
                    boundary: ComputerAccessBoundary::Unknown,
                    capabilities: capabilities_for_boundary(
                        ComputerAccessBoundary::Unknown,
                        "window id is not a valid HWND",
                    ),
                    reason: Some("window id is not a valid HWND".into()),
                }));
                window
            })
            .collect())
    }

    fn window_security_metadata(&self, hwnd: HWND) -> WindowSecurityMetadata {
        match self.security_decision_for_hwnd(hwnd, "window_observation") {
            Ok(decision) => WindowSecurityMetadata {
                process: decision.target.clone(),
                boundary: decision.boundary,
                capabilities: decision.capabilities,
                reason: Some(decision.reason),
            },
            Err(error) => {
                let boundary = match error {
                    ComputerError::TargetUnavailable(_) => {
                        ComputerAccessBoundary::TargetUnavailable
                    }
                    ComputerError::ProtectedDesktop(_) => ComputerAccessBoundary::ProtectedDesktop,
                    _ => ComputerAccessBoundary::Unknown,
                };
                let reason = error.to_string();
                WindowSecurityMetadata {
                    process: None,
                    boundary,
                    capabilities: capabilities_for_boundary(boundary, &reason),
                    reason: Some(reason),
                }
            }
        }
    }

    fn send_pointer_move(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
    ) -> Result<String, ComputerError> {
        let (x, y, detail) = self.coordinate(session, coordinate)?;
        let pointer_detail = self.move_pointer_verified(x, y)?;
        Ok(format!("{detail}; {pointer_detail}"))
    }

    fn absolute_mouse_move(&self, x: i32, y: i32) -> Result<INPUT, ComputerError> {
        // Use the same physical topology that produced the screenshot and its
        // desktop_origin. GetSystemMetrics is DPI-virtualized for an unaware
        // process/thread; mixing that logical span with physical screenshot
        // pixels makes 150% displays overshoot and can clamp lower-screen
        // clicks to the desktop edge.
        let bounds = &self.topology()?.topology.virtual_desktop_bounds;
        absolute_mouse_move_in_bounds(x, y, bounds)
    }

    fn move_pointer_verified(&self, x: i32, y: i32) -> Result<String, ComputerError> {
        unsafe {
            send_input(&[self.absolute_mouse_move(x, y)?])?;
        }
        let deadline = Instant::now() + Duration::from_millis(50);
        let mut actual = POINT {
            x: i32::MIN,
            y: i32::MIN,
        };
        loop {
            unsafe {
                GetCursorPos(&mut actual).map_err(|error| {
                    ComputerError::Backend(format!(
                        "GetCursorPos failed while verifying ({x},{y}): {error}"
                    ))
                })?;
            }
            if (i64::from(actual.x) - i64::from(x)).abs() <= 1
                && (i64::from(actual.y) - i64::from(y)).abs() <= 1
            {
                return Ok(format!(
                    "pointer_requested=({x}, {y}); pointer_observed=({}, {}); pointer_verified=true",
                    actual.x, actual.y
                ));
            }
            if Instant::now() >= deadline {
                return Err(ComputerError::Backend(format!(
                    "pointer placement verification failed before consequential input: requested=({x},{y}) observed=({},{})",
                    actual.x, actual.y
                )));
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn send_mouse_button(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        button: MouseButton,
        clicks: u32,
    ) -> Result<String, ComputerError> {
        let (x, y, detail) = self.coordinate(session, coordinate)?;
        let (down, up) = mouse_button_flags(button);
        let mut events = Vec::with_capacity(clicks as usize * 2);
        for _ in 0..clicks {
            events.push(mouse_input(down));
            events.push(mouse_input(up));
        }
        let pointer_detail = self.move_pointer_verified(x, y)?;
        unsafe {
            send_input(&events)?;
        }
        Ok(format!("{detail}; {pointer_detail}; clicks={clicks}"))
    }

    fn send_mouse_transition(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        button: MouseButton,
        down: bool,
    ) -> Result<String, ComputerError> {
        let (x, y, detail) = self.coordinate(session, coordinate)?;
        let (down_flags, up_flags) = mouse_button_flags(button);
        let flags = if down { down_flags } else { up_flags };
        let pointer_detail = self.move_pointer_verified(x, y)?;
        unsafe {
            if let Err(error) = send_input(&[mouse_input(flags)]) {
                if down {
                    let _ = send_input(&[mouse_input(up_flags)]);
                }
                return Err(error);
            }
        }
        Ok(format!(
            "{detail}; {pointer_detail}; button={button:?}; phase={}",
            if down { "down" } else { "up" }
        ))
    }

    fn send_modifier_click(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        modifier: &str,
        button: MouseButton,
    ) -> Result<String, ComputerError> {
        let modifier = modifier_vk(modifier).ok_or_else(|| {
            ComputerError::InvalidAction(format!(
                "unsupported modifier for ModifierClick: {modifier}"
            ))
        })?;
        let (x, y, detail) = self.coordinate(session, coordinate)?;
        let (button_down, button_up) = mouse_button_flags(button);
        let events = [
            virtual_key_input(modifier, false),
            mouse_input(button_down),
            mouse_input(button_up),
            virtual_key_input(modifier, true),
        ];
        let pointer_detail = self.move_pointer_verified(x, y)?;
        unsafe {
            if let Err(error) = send_input(&events) {
                // The transaction may have inserted only a prefix. Release
                // both possible held inputs; this is best effort and leaves no
                // pressed state in the session on the normal path.
                let _ = send_input(&[mouse_input(button_up), virtual_key_input(modifier, true)]);
                return Err(error);
            }
        }
        Ok(format!(
            "{detail}; {pointer_detail}; button={button:?}; modifier={modifier:?}; transaction=true"
        ))
    }

    fn pressed_state_mut(&mut self, session: &ComputerSessionId) -> &mut PressedState {
        self.pressed.entry(session.clone()).or_default()
    }

    fn send_key_transition(
        &self,
        key: &str,
        down: bool,
    ) -> Result<(VIRTUAL_KEY, String), ComputerError> {
        let virtual_key = physical_vk(key)?;
        unsafe {
            if let Err(error) = send_input(&[key_input(virtual_key, !down)]) {
                if down {
                    let _ = send_input(&[key_input(virtual_key, true)]);
                }
                return Err(error);
            }
        }
        Ok((
            virtual_key,
            format!(
                "virtual_key=0x{:02x}; phase={}",
                virtual_key.0,
                if down { "down" } else { "up" }
            ),
        ))
    }

    fn release_pressed(&mut self, session: &ComputerSessionId) {
        let Some(state) = self.pressed.remove(session) else {
            return;
        };
        let mut events = Vec::with_capacity(state.keys.len() + state.mouse_buttons.len());
        for key in state.keys {
            if let Ok(virtual_key) = physical_vk(&key) {
                events.push(key_input(virtual_key, true));
            }
        }
        for button in state.mouse_buttons {
            let (_, up) = mouse_button_flags(button);
            events.push(mouse_input(up));
        }
        if !events.is_empty() {
            unsafe {
                let _ = send_input(&events);
            }
        }
    }

    fn send_drag(
        &self,
        session: &ComputerSessionId,
        from: &Coordinate,
        to: &Coordinate,
        button: MouseButton,
    ) -> Result<String, ComputerError> {
        let (from_x, from_y, from_detail) = self.coordinate(session, from)?;
        let (to_x, to_y, to_detail) = self.coordinate(session, to)?;
        let (down, up) = mouse_button_flags(button);
        let from_pointer_detail = self.move_pointer_verified(from_x, from_y)?;
        let to_move = self.absolute_mouse_move(to_x, to_y)?;
        unsafe {
            send_input(&[mouse_input(down)])?;
            if let Err(error) = send_input(&[to_move]) {
                let _ = send_input(&[mouse_input(up)]);
                return Err(error);
            }
            if let Err(error) = send_input(&[mouse_input(up)]) {
                // The down event may already have reached the desktop. A
                // failed up must never leave the session with a held button.
                let _ = send_input(&[mouse_input(up)]);
                return Err(error);
            }
        }
        Ok(format!(
            "from={from_detail} ({from_x},{from_y}); {from_pointer_detail}; to={to_detail} ({to_x},{to_y}); button={button:?}"
        ))
    }

    fn send_scroll(
        &self,
        session: &ComputerSessionId,
        coordinate: &Coordinate,
        direction: ScrollDirection,
        amount: u32,
    ) -> Result<String, ComputerError> {
        let (x, y, detail) = self.coordinate(session, coordinate)?;
        let amount = amount.min(i32::MAX as u32) as i32;
        let (flags, signed_amount) = match direction {
            ScrollDirection::Up => (MOUSEEVENTF_WHEEL, amount),
            ScrollDirection::Down => (MOUSEEVENTF_WHEEL, -amount),
            ScrollDirection::Left => (MOUSEEVENTF_HWHEEL, -amount),
            ScrollDirection::Right => (MOUSEEVENTF_HWHEEL, amount),
        };
        let pointer_detail = self.move_pointer_verified(x, y)?;
        unsafe {
            send_input(&[mouse_input_with_data(flags, signed_amount as u32)])?;
        }
        Ok(format!(
            "{detail}; {pointer_detail}; direction={direction:?}; amount={amount}"
        ))
    }

    pub fn capture_primary_profile(
        &self,
        png_mode: NativePngMode,
    ) -> Result<NativeCaptureProfile, ComputerError> {
        let started = Instant::now();
        let info = self.primary_display_info()?;
        let width = info.monitor.size.width.round() as i32;
        let height = info.monitor.size.height.round() as i32;
        if width <= 0 || height <= 0 {
            return Err(ComputerError::Backend(
                "primary monitor has invalid size".into(),
            ));
        }
        let gdi = capture_gdi_bgra(
            info.monitor.origin.x.round() as i32,
            info.monitor.origin.y.round() as i32,
            width,
            height,
        )?;
        let (bytes, pixel_conversion_micros, png_encode_micros) =
            encode_bgra_png_with_mode(&gdi.bgra, gdi.width, gdi.height, gdi.stride, png_mode)?;
        let png_bytes = bytes.len();
        Ok(NativeCaptureProfile {
            screenshot: Screenshot {
                metadata: ScreenshotMetadata {
                    frame_id: FrameId::new(format!(
                        "profile-{}",
                        SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    )),
                    display_id: Some(info.display_id.clone()),
                    width: width as u32,
                    height: height as u32,
                    mime_type: "image/png".into(),
                    coordinate_space: CoordinateSpace::ScreenshotPixel,
                    desktop_origin: info.monitor.origin,
                    dpi: info.dpi,
                    scale: info.scale,
                    captured_at: Some(SystemTime::now()),
                },
                bytes,
            },
            capture_micros: gdi.capture_micros,
            pixel_readback_micros: gdi.pixel_readback_micros,
            pixel_conversion_micros,
            png_encode_micros,
            total_micros: started.elapsed().as_micros(),
            png_bytes,
        })
    }

    fn frame_store_mut(&mut self, session: &ComputerSessionId) -> &mut FrameStore {
        self.frames.entry(session.clone()).or_default()
    }

    fn capture_display_frame(
        &mut self,
        session: &ComputerSessionId,
        display_id: &DisplayId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        self.validate_display_id(display_id)?;
        let (topology_generation, display) = {
            let topology = self.topology()?;
            (
                topology.topology.topology_generation,
                topology
                    .topology
                    .display(display_id)
                    .map_err(ComputerError::from)?
                    .clone(),
            )
        };
        let width = display.physical_size.width.round() as i32;
        let height = display.physical_size.height.round() as i32;
        if width <= 0 || height <= 0 {
            return Err(ComputerError::InvalidCoordinate(format!(
                "display {display_id} has invalid physical size"
            )));
        }
        let gdi = capture_gdi_bgra(
            display.physical_bounds.origin.x.round() as i32,
            display.physical_bounds.origin.y.round() as i32,
            width,
            height,
        )?;
        let frame_id = FrameId::new(format!("frame-{}-{}", topology_generation, self.next_frame));
        self.next_frame = self.next_frame.saturating_add(1);
        let metadata = CaptureFrameMetadata {
            frame_id: frame_id.clone(),
            display_id: display.id.clone(),
            topology_generation,
            coordinate_space: CoordinateSpace::ScreenshotPixel,
            desktop_origin: display.physical_bounds.origin,
            dpi: display.dpi,
            scale: display.scale,
            width: gdi.width,
            height: gdi.height,
            pixel_format: FramePixelFormat::Bgra8,
            stride: gdi.stride,
            captured_at: Some(SystemTime::now()),
            content_revision: self.next_frame,
            stale_topology: false,
        };
        let store_started = Instant::now();
        self.frame_store_mut(session).insert(StoredFrame {
            metadata: metadata.clone(),
            pixels: gdi.bgra,
            encoded_png: None,
        })?;
        let store = self.frame_store_mut(session);
        store.capture_count = store.capture_count.saturating_add(1);
        store.total_capture_micros = store
            .total_capture_micros
            .saturating_add(gdi.capture_micros);
        store.total_store_micros = store
            .total_store_micros
            .saturating_add(store_started.elapsed().as_micros());
        Ok(metadata)
    }

    fn encode_display_frame(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameEncodingResult, ComputerError> {
        let topology_generation = self.topology()?.topology.topology_generation;
        let started = Instant::now();
        let (screenshot, cache_hit) = self
            .frame_store_mut(session)
            .encode(frame_id, topology_generation)?;
        Ok(FrameEncodingResult {
            screenshot,
            encoding: FrameEncoding::Png,
            cache_hit,
            encode_micros: started.elapsed().as_micros(),
        })
    }

    fn capture_display(
        &mut self,
        session: &ComputerSessionId,
        display_id: &DisplayId,
    ) -> Result<Screenshot, ComputerError> {
        let frame = self.capture_display_frame(session, display_id)?;
        Ok(self
            .encode_display_frame(session, &frame.frame_id)?
            .screenshot)
    }
}

fn desktop_name(handle: HANDLE) -> Result<String, ComputerError> {
    unsafe {
        let mut bytes_needed = 0u32;
        let _ = GetUserObjectInformationW(handle, UOI_NAME, None, 0, Some(&mut bytes_needed));
        if bytes_needed < 2 {
            return Err(ComputerError::SecurityContextUnavailable(
                "GetUserObjectInformationW(UOI_NAME) returned no name buffer".into(),
            ));
        }
        let mut buffer = vec![0u16; (bytes_needed as usize).div_ceil(2)];
        GetUserObjectInformationW(
            handle,
            UOI_NAME,
            Some(buffer.as_mut_ptr() as *mut _),
            bytes_needed,
            Some(&mut bytes_needed),
        )
        .map_err(|error| {
            ComputerError::SecurityContextUnavailable(format!(
                "GetUserObjectInformationW(UOI_NAME) failed: {error}"
            ))
        })?;
        let length = buffer
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(buffer.len());
        Ok(String::from_utf16_lossy(&buffer[..length]))
    }
}

fn integrity_rank(level: IntegrityLevel) -> u8 {
    match level {
        IntegrityLevel::Untrusted => 0,
        IntegrityLevel::Low => 1,
        IntegrityLevel::Medium => 2,
        IntegrityLevel::MediumPlus => 3,
        IntegrityLevel::High => 4,
        IntegrityLevel::System => 5,
        IntegrityLevel::Unknown => 255,
    }
}

fn boundary_reason(
    boundary: ComputerAccessBoundary,
    source: &ProcessSecurityContext,
    target: &ProcessSecurityContext,
    desktop: &DesktopSecurityContext,
) -> String {
    match boundary {
        ComputerAccessBoundary::Allowed => format!(
            "target integrity {:?} is not higher than sidecar integrity {:?}; desktop={:?}",
            target.integrity_level, source.integrity_level, desktop.desktop_kind
        ),
        ComputerAccessBoundary::ElevationRequired => format!(
            "target process {} requires elevation: target integrity {:?} > sidecar integrity {:?}",
            target
                .process_id
                .map(|pid| pid.to_string())
                .unwrap_or_else(|| "unknown".into()),
            target.integrity_level,
            source.integrity_level
        ),
        ComputerAccessBoundary::ProtectedDesktop => {
            "current input desktop is protected or secure; side effects are denied".into()
        }
        ComputerAccessBoundary::Unknown => {
            "source or target security context is unknown; action fails closed".into()
        }
        ComputerAccessBoundary::IntegrityMismatch => {
            "source and target integrity levels are incompatible".into()
        }
        ComputerAccessBoundary::TargetUnavailable => "target window is unavailable".into(),
    }
}

fn capabilities_for_boundary(
    boundary: ComputerAccessBoundary,
    reason: &str,
) -> TargetAccessCapabilities {
    let allowed = || CapabilityAccess::Allowed;
    let denied = || CapabilityAccess::Denied {
        reason: reason.to_owned(),
    };
    let unknown = || CapabilityAccess::Unknown {
        reason: reason.to_owned(),
    };
    match boundary {
        ComputerAccessBoundary::Allowed => TargetAccessCapabilities {
            pixel_observation: allowed(),
            semantic_observation: allowed(),
            window_focus: allowed(),
            pointer_input: allowed(),
            keyboard_input: allowed(),
            semantic_action: allowed(),
        },
        ComputerAccessBoundary::ElevationRequired
        | ComputerAccessBoundary::IntegrityMismatch
        | ComputerAccessBoundary::ProtectedDesktop => TargetAccessCapabilities {
            pixel_observation: allowed(),
            semantic_observation: unknown(),
            window_focus: denied(),
            pointer_input: denied(),
            keyboard_input: denied(),
            semantic_action: denied(),
        },
        ComputerAccessBoundary::TargetUnavailable | ComputerAccessBoundary::Unknown => {
            TargetAccessCapabilities {
                pixel_observation: unknown(),
                semantic_observation: unknown(),
                window_focus: denied(),
                pointer_input: denied(),
                keyboard_input: denied(),
                semantic_action: denied(),
            }
        }
    }
}

fn token_information(
    token: HANDLE,
    class: windows::Win32::Security::TOKEN_INFORMATION_CLASS,
) -> Result<Vec<u8>, ComputerError> {
    unsafe {
        let mut length = 0u32;
        let _ = GetTokenInformation(token, class, None, 0, &mut length);
        if length == 0 {
            return Err(ComputerError::SecurityContextUnavailable(format!(
                "GetTokenInformation({class:?}) returned no buffer length"
            )));
        }
        let mut buffer = vec![0u8; length as usize];
        GetTokenInformation(
            token,
            class,
            Some(buffer.as_mut_ptr() as *mut _),
            length,
            &mut length,
        )
        .map_err(|error| {
            ComputerError::SecurityContextUnavailable(format!(
                "GetTokenInformation({class:?}) failed: {error}"
            ))
        })?;
        Ok(buffer)
    }
}

fn inspect_process_security(process_id: u32) -> Result<ProcessSecurityContext, ComputerError> {
    unsafe {
        let process =
            OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id).map_err(|error| {
                ComputerError::SecurityContextUnavailable(format!(
                    "OpenProcess({process_id}) failed: {error}"
                ))
            })?;
        let mut token = HANDLE::default();
        if let Err(error) = OpenProcessToken(process, TOKEN_QUERY, &mut token) {
            let _ = CloseHandle(process);
            return Err(ComputerError::SecurityContextUnavailable(format!(
                "OpenProcessToken({process_id}) failed: {error}"
            )));
        }
        let result = (|| {
            let integrity = token_information(token, TokenIntegrityLevel)?;
            let label = &*(integrity.as_ptr() as *const TOKEN_MANDATORY_LABEL);
            if label.Label.Sid.0.is_null() {
                return Err(ComputerError::SecurityContextUnavailable(
                    "TokenIntegrityLevel returned a null SID".into(),
                ));
            }
            let count = *GetSidSubAuthorityCount(label.Label.Sid);
            if count == 0 {
                return Err(ComputerError::SecurityContextUnavailable(
                    "TokenIntegrityLevel returned an empty SID".into(),
                ));
            }
            let rid = *GetSidSubAuthority(label.Label.Sid, (count - 1) as u32);
            let integrity_level = match rid {
                0 => IntegrityLevel::Untrusted,
                4096 => IntegrityLevel::Low,
                8192 => IntegrityLevel::Medium,
                8448 => IntegrityLevel::MediumPlus,
                12288 => IntegrityLevel::High,
                16384 => IntegrityLevel::System,
                value if value < 4096 => IntegrityLevel::Untrusted,
                value if value < 8192 => IntegrityLevel::Low,
                value if value < 12288 => IntegrityLevel::Medium,
                value if value < 16384 => IntegrityLevel::High,
                _ => IntegrityLevel::System,
            };
            let elevation = token_information(token, TokenElevation)?;
            let elevated = (*(elevation.as_ptr() as *const TOKEN_ELEVATION)).TokenIsElevated != 0;
            let ui_access = token_information(token, TokenUIAccess)?;
            let ui_access = *(ui_access.as_ptr() as *const u32) != 0;
            let app_container = token_information(token, TokenIsAppContainer)?;
            let app_container = *(app_container.as_ptr() as *const u32) != 0;
            Ok(ProcessSecurityContext {
                integrity_level,
                elevated,
                ui_access,
                app_container,
                process_id: Some(process_id),
            })
        })();
        let _ = CloseHandle(token);
        let _ = CloseHandle(process);
        result
    }
}

fn inspect_hwnd_security(hwnd: HWND) -> Result<(u32, ProcessSecurityContext), ComputerError> {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return Err(ComputerError::TargetUnavailable(
                "target HWND is no longer valid".into(),
            ));
        }
        let mut process_id = 0u32;
        if GetWindowThreadProcessId(hwnd, Some(&mut process_id)) == 0 || process_id == 0 {
            return Err(ComputerError::TargetUnavailable(
                "target HWND has no owning process".into(),
            ));
        }
        let context = inspect_process_security(process_id)?;
        let mut after_process_id = 0u32;
        if !IsWindow(hwnd).as_bool()
            || GetWindowThreadProcessId(hwnd, Some(&mut after_process_id)) == 0
            || after_process_id != process_id
        {
            return Err(ComputerError::TargetUnavailable(
                "target HWND exited or changed process identity during inspection".into(),
            ));
        }
        Ok((process_id, context))
    }
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
    }
}

impl Default for WinNativeBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl WinNativeBackend {
    fn cached_semantic_assessment(
        &self,
        session: &ComputerSessionId,
        action: &SemanticAction,
        security: Option<&SecurityDecision>,
    ) -> Option<CapabilityAssessment> {
        let window_id = self.uia.element_window(session, action.element_id()).ok()?;
        let generation = self.uia.current_generation(session)?;
        let entry = self
            .capability_cache
            .get(session)?
            .entries
            .get(&window_id)?;
        if entry.semantic_generation != generation
            || security.is_some_and(|decision| {
                entry.profile.environment.access_boundary != decision.boundary
            })
        {
            return None;
        }
        Some(entry.profile.semantic_action(action).clone())
    }

    async fn prepare_pixel_fallback(
        &mut self,
        session: &ComputerSessionId,
        display: &NativeDisplayInfo,
        action: &SemanticAction,
    ) -> Result<
        (
            PixelTarget,
            ComputerAction,
            Vec<alice_computer_use_core::ComputerElement>,
        ),
        ComputerError,
    > {
        let element_id = action.element_id();
        let target = self.uia.pixel_target(session, element_id, display)?;
        let (snapshot, generation) = self.uia.current_snapshot(session)?;
        if generation != target.generation {
            return Err(ComputerError::StaleElement(
                "pixel target generation changed before fallback admission".into(),
            ));
        }
        let windows = self.enumerate_window_records(display)?;
        let window = windows
            .iter()
            .find(|window| window.id == target.window_id)
            .ok_or_else(|| {
                ComputerError::InvalidWindow("pixel target window disappeared".into())
            })?;
        if !window.active {
            return Err(ComputerError::ForegroundDenied(
                "pixel fallback target window is not foreground; fallback stopped".into(),
            ));
        }
        let pixel_action = match action {
            SemanticAction::SetValue { value, .. } => ComputerAction::TypeText {
                text: value.clone(),
                target: Some(target.window_id.clone()),
                at: None,
            },
            _ => ComputerAction::Click {
                at: target.center.clone(),
            },
        };
        Ok((target.clone(), pixel_action, snapshot))
    }

    async fn run_pixel_attempt(
        &mut self,
        session: &ComputerSessionId,
        display: &NativeDisplayInfo,
        window_id: &WindowId,
        action: &ComputerAction,
        before_snapshot: Option<&[alice_computer_use_core::ComputerElement]>,
    ) -> PixelAttemptResult {
        let windows = match self.enumerate_window_records(display) {
            Ok(windows) => windows,
            Err(error) => {
                return PixelAttemptResult::error(
                    execution_outcome_from_error(&error),
                    error.to_string(),
                )
            }
        };
        let Some(window) = windows.iter().find(|window| window.id == *window_id) else {
            return PixelAttemptResult::error(
                ComputerExecutionOutcome::InvalidTarget,
                "pixel target window disappeared before click".into(),
            );
        };
        if !window.active {
            return PixelAttemptResult::error(
                ComputerExecutionOutcome::FocusDenied,
                "pixel target window is not foreground; pointer action rejected".into(),
            );
        }
        let executed = match self.execute(session, action).await {
            Ok(executed) => executed,
            Err(error) => {
                return PixelAttemptResult::error(
                    execution_outcome_from_error(&error),
                    error.to_string(),
                )
            }
        };
        let dispatch_detail = executed
            .backend_detail
            .unwrap_or_else(|| "backend dispatch completed without detail".into());
        if external_input_action_requires_fixture_evidence(action) {
            return PixelAttemptResult {
                outcome: ComputerExecutionOutcome::Performed,
                verification: ComputerExecutionVerification {
                    kind: Some(ComputerExecutionVerificationKind::CustomFixtureState),
                    verified: false,
                    detail: Some(format!(
                        "explicit input was dispatched; caller must verify application state or logger evidence; dispatch={dispatch_detail}"
                    )),
                    ..Default::default()
                },
                generation_after: None,
                verification_ms: 0,
                detail: Some("explicit input dispatch requires external application evidence".into()),
            };
        }
        let Some(before_snapshot) = before_snapshot else {
            return PixelAttemptResult {
                outcome: ComputerExecutionOutcome::VerificationFailed,
                verification: ComputerExecutionVerification {
                    kind: Some(ComputerExecutionVerificationKind::CustomFixtureState),
                    detail: Some(
                        "pixel action succeeded but no semantic verifier target was supplied"
                            .into(),
                    ),
                    ..Default::default()
                },
                generation_after: None,
                verification_ms: 0,
                detail: Some("pixel API success was not treated as verified".into()),
            };
        };
        let opaque_semantic_target = semantic_snapshot_is_opaque(before_snapshot);
        let hwnd = match Self::hwnd(window_id) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                return PixelAttemptResult {
                    outcome: ComputerExecutionOutcome::OutcomeUnknown,
                    verification: ComputerExecutionVerification {
                        kind: Some(ComputerExecutionVerificationKind::CustomFixtureState),
                        detail: Some(error.to_string()),
                        ..Default::default()
                    },
                    generation_after: None,
                    verification_ms: 0,
                    detail: Some(
                        "pixel action was sent but refresh could not resolve its window".into(),
                    ),
                }
            }
        };
        let verify_started = Instant::now();
        let mut last_observation = None;
        let mut last_error = None;
        for attempt in 0..=VERIFICATION_RETRY_DELAYS_MS.len() {
            if attempt > 0 {
                std::thread::sleep(Duration::from_millis(
                    VERIFICATION_RETRY_DELAYS_MS[attempt - 1],
                ));
            }
            match self.uia.observe(
                display,
                hwnd,
                session,
                window_id,
                SemanticObservationLimits::default(),
            ) {
                Ok(observation) => {
                    if UiaStore::semantic_state_changed(before_snapshot, &observation) {
                        return PixelAttemptResult {
                            outcome: ComputerExecutionOutcome::Performed,
                            verification: ComputerExecutionVerification {
                                kind: Some(
                                    ComputerExecutionVerificationKind::SemanticStateChanged,
                                ),
                                verified: true,
                                state_changed: Some(true),
                                detail: Some(format!(
                                    "pixel action was dispatched once and semantic state changed after {} observation(s)",
                                    attempt + 1
                                )),
                                ..Default::default()
                            },
                            generation_after: Some(observation.metadata.generation),
                            verification_ms: verify_started.elapsed().as_millis(),
                            detail: None,
                        };
                    }
                    last_observation = Some(observation);
                }
                Err(error) => last_error = Some(error),
            }
        }
        if let Some(observation) = last_observation {
            if opaque_semantic_target && semantic_snapshot_is_opaque(&observation.elements) {
                return PixelAttemptResult {
                    outcome: ComputerExecutionOutcome::Performed,
                    verification: ComputerExecutionVerification {
                        kind: Some(ComputerExecutionVerificationKind::CustomFixtureState),
                        verified: false,
                        state_changed: Some(false),
                        detail: Some(format!(
                            "input dispatch completed without an input conflict, but the target exposes only an opaque UIA root/pane tree; application effect is unverified and should be checked with a fresh screenshot before any consequential follow-up; dispatch={dispatch_detail}"
                        )),
                        ..Default::default()
                    },
                    generation_after: Some(observation.metadata.generation),
                    verification_ms: verify_started.elapsed().as_millis(),
                    detail: Some(
                        "input was dispatched once to a semantically opaque modern application"
                            .into(),
                    ),
                };
            }
            PixelAttemptResult {
                outcome: ComputerExecutionOutcome::VerificationFailed,
                verification: ComputerExecutionVerification {
                    kind: Some(ComputerExecutionVerificationKind::SemanticStateChanged),
                    verified: false,
                    state_changed: Some(false),
                    detail: Some(format!(
                        "pixel action was dispatched once but no semantic state change was observed after {} bounded observations; do not replay without a fresh observation",
                        VERIFICATION_RETRY_DELAYS_MS.len() + 1
                    )),
                    ..Default::default()
                },
                generation_after: Some(observation.metadata.generation),
                verification_ms: verify_started.elapsed().as_millis(),
                detail: Some("action dispatch succeeded but its effect was not proven".into()),
            }
        } else {
            let error = last_error
                .map(|error| error.to_string())
                .unwrap_or_else(|| "semantic verification produced no observation".into());
            PixelAttemptResult {
                outcome: ComputerExecutionOutcome::OutcomeUnknown,
                verification: ComputerExecutionVerification {
                    kind: Some(ComputerExecutionVerificationKind::SemanticStateChanged),
                    detail: Some(format!(
                        "pixel action was sent but semantic verification failed: {error}"
                    )),
                    ..Default::default()
                },
                generation_after: None,
                verification_ms: verify_started.elapsed().as_millis(),
                detail: Some("pixel action outcome could not be verified".into()),
            }
        }
    }
}

#[async_trait]
impl ComputerBackend for WinNativeBackend {
    async fn initialize(&mut self) -> Result<(), ComputerError> {
        if self.initialized {
            return Err(ComputerError::AlreadyInitialized);
        }
        unsafe {
            let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            self.dpi = GetDpiForSystem().max(1);
        }
        self.self_security = Some(inspect_process_security(unsafe { GetCurrentProcessId() })?);
        self.desktop_security_context_now()?;
        self.initialized = true;
        if let Err(error) = self.refresh_topology() {
            self.initialized = false;
            return Err(error);
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> Result<(), ComputerError> {
        let sessions = self.pressed.keys().cloned().collect::<Vec<_>>();
        for session in sessions {
            self.release_pressed(&session);
        }
        self.pressed.clear();
        self.initialized = false;
        self.uia.clear();
        self.topology = None;
        self.frames.clear();
        self.capability_cache.clear();
        self.self_security = None;
        Ok(())
    }

    async fn security_context(&mut self) -> Result<ProcessSecurityContext, ComputerError> {
        self.ensure_initialized()?;
        self.self_security_context()
    }

    async fn desktop_security_context(&mut self) -> Result<DesktopSecurityContext, ComputerError> {
        self.ensure_initialized()?;
        self.desktop_security_context_now()
    }

    fn capabilities(&self) -> Vec<Capability> {
        vec![
            Capability {
                name: "focus_window",
                state: CapabilityState::Supported,
                detail: "Win32 ShowWindow/BringWindowToTop/SetForegroundWindow with foreground confirmation",
            },
            Capability {
                name: "pointer_click",
                state: CapabilityState::Supported,
                detail: "Win32 marked absolute SendInput mouse positioning and button events",
            },
            Capability {
                name: "unicode_text",
                state: CapabilityState::Supported,
                detail: "SendInput KEYEVENTF_UNICODE over UTF-16 code units",
            },
            Capability {
                name: "physical_key",
                state: CapabilityState::Supported,
                detail: "SendInput scan-code keyboard events",
            },
            Capability {
                name: "modifier_hotkey",
                state: CapabilityState::Supported,
                detail: "SendInput modifier-down/key-down/key-up/modifier-up transaction with failure release",
            },
            Capability {
                name: "explicit_input_state",
                state: CapabilityState::Supported,
                detail: "Session-scoped MouseDown/MouseUp and KeyDown/KeyUp state with graceful cleanup",
            },
            Capability {
                name: "window_enumeration",
                state: CapabilityState::Supported,
                detail: "Win32 EnumWindows with visible top-level title, bounds, PID, and foreground state",
            },
            Capability {
                name: "display_topology",
                state: CapabilityState::Supported,
                detail: "Win32 EnumDisplayMonitors with physical bounds, work area, DPI, scale, primary identity, and virtual desktop bounds",
            },
            Capability {
                name: "screenshot",
                state: CapabilityState::Supported,
                detail: "Win32 GDI per-display desktop capture encoded as standard PNG with explicit frame/display metadata",
            },
            Capability {
                name: "observation",
                state: CapabilityState::Supported,
                detail: "Native display topology/window enumeration, screenshot, and bounded UIA semantic observation",
            },
            Capability {
                name: "semantic_observation",
                state: CapabilityState::Supported,
                detail: "Read-only Windows UI Automation control-view tree with bounded properties and session-scoped opaque element ids",
            },
            Capability {
                name: "semantic_action",
                state: CapabilityState::Supported,
                detail: "UIA Focus, Invoke, ValuePattern, Toggle, SelectionItem, ExpandCollapse, RangeValue, and ScrollItem actions with admission and readback verification",
            },
            Capability {
                name: "security_context",
                state: CapabilityState::Supported,
                detail: "Process integrity/elevation/UIAccess/AppContainer diagnostics with no raw token data",
            },
            Capability {
                name: "target_access_boundary",
                state: CapabilityState::Supported,
                detail: "Target integrity and capability admission before side-effecting actions",
            },
            Capability {
                name: "desktop_security_context",
                state: CapabilityState::Supported,
                detail: "Interactive versus protected/secure input desktop classification",
            },
        ]
    }

    async fn enumerate_screens(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<Vec<Screen>, ComputerError> {
        self.refresh_topology()?;
        Ok(self.topology()?.topology.displays.clone())
    }

    async fn display_topology(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<DisplayTopology, ComputerError> {
        self.refresh_topology()?;
        Ok(self.topology()?.topology.clone())
    }

    async fn enumerate_windows(
        &mut self,
        _session: &ComputerSessionId,
    ) -> Result<Vec<Window>, ComputerError> {
        self.refresh_topology()?;
        let display = self.primary_display_info()?;
        self.enumerate_window_records(&display)
    }

    async fn screenshot(
        &mut self,
        session: &ComputerSessionId,
        screen: &ScreenId,
    ) -> Result<Screenshot, ComputerError> {
        self.refresh_topology()?;
        self.capture_display(session, screen)
    }

    async fn capture_frame(
        &mut self,
        session: &ComputerSessionId,
        screen: &ScreenId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        self.refresh_topology()?;
        self.capture_display_frame(session, screen)
    }

    async fn frame_metadata(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        self.refresh_topology()?;
        Ok(self
            .frames
            .get(session)
            .map(|store| {
                store.state(
                    frame_id,
                    self.topology().unwrap().topology.topology_generation,
                )
            })
            .unwrap_or(FrameMetadataResult {
                metadata: None,
                state: FrameState::Unknown,
            }))
    }

    async fn encode_frame(
        &mut self,
        session: &ComputerSessionId,
        frame_id: &FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, ComputerError> {
        self.refresh_topology()?;
        if encoding != FrameEncoding::Png {
            return Err(ComputerError::Unsupported {
                capability: "frame_encoding".into(),
                detail: "only PNG is implemented in R7".into(),
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
            .get_mut(session)
            .map(|store| store.release(frame_id))
            .unwrap_or(FrameMetadataResult {
                metadata: None,
                state: FrameState::Unknown,
            }))
    }

    async fn screenshot_target(
        &mut self,
        _session: &ComputerSessionId,
        target: &ScreenshotTarget,
    ) -> Result<Screenshot, ComputerError> {
        self.refresh_topology()?;
        match target {
            ScreenshotTarget::Display(display_id) => self.capture_display(_session, display_id),
            ScreenshotTarget::VirtualDesktop => Err(ComputerError::Unsupported {
                capability: "virtual_desktop_screenshot".into(),
                detail: "virtual desktop capture is not implemented in R6".into(),
            }),
        }
    }

    async fn execute(
        &mut self,
        session: &ComputerSessionId,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        self.preflight_action(session, action)?;
        let (detail, action_name) = match action {
            ComputerAction::FocusWindow { window_id } => {
                self.focus(window_id)?;
                (
                    format!("hwnd={} foreground_confirmed=true", window_id),
                    "focus_window",
                )
            }
            ComputerAction::MovePointer { to } => {
                (self.send_pointer_move(session, to)?, "move_pointer")
            }
            ComputerAction::Click { at } => (
                self.send_mouse_button(session, at, MouseButton::Left, 1)?,
                "click",
            ),
            ComputerAction::DoubleClick { at } => (
                self.send_mouse_button(session, at, MouseButton::Left, 2)?,
                "double_click",
            ),
            ComputerAction::RightClick { at } => (
                self.send_mouse_button(session, at, MouseButton::Right, 1)?,
                "right_click",
            ),
            ComputerAction::Drag { from, to, button } => {
                (self.send_drag(session, from, to, *button)?, "drag")
            }
            ComputerAction::Scroll {
                at,
                direction,
                amount,
            } => (
                self.send_scroll(session, at, *direction, *amount)?,
                "scroll",
            ),
            ComputerAction::TypeText { text, target, at } => {
                self.foreground_target(target.as_ref())?;
                let click_detail = if let Some(at) = at {
                    let detail = self.send_mouse_button(session, at, MouseButton::Left, 1)?;
                    // SendInput insertion is synchronous, application focus
                    // handling is not. Give the target queue one short bounded
                    // settle interval, then re-check the foreground binding
                    // before injecting any text.
                    thread::sleep(Duration::from_millis(40));
                    self.foreground_target(target.as_ref())?;
                    Some(detail)
                } else {
                    None
                };
                let text_detail = self.send_text(text)?;
                (
                    match click_detail {
                        Some(click_detail) => format!(
                            "atomic_click_then_type=true; {click_detail}; focus_settle_ms=40; {text_detail}"
                        ),
                        None => text_detail,
                    },
                    "type_text",
                )
            }
            ComputerAction::KeyPress { key, target } => {
                self.foreground_target(target.as_ref())?;
                (self.send_key(key)?, "key_press")
            }
            ComputerAction::Hotkey { keys, target } => {
                self.foreground_target(target.as_ref())?;
                (self.send_hotkey(keys)?, "hotkey")
            }
            ComputerAction::MouseDown { button, at, target } => {
                self.foreground_target(target.as_ref())?;
                let detail = self.send_mouse_transition(session, at, *button, true)?;
                self.pressed_state_mut(session)
                    .mouse_buttons
                    .insert(*button);
                (detail, "mouse_down")
            }
            ComputerAction::MouseUp { button, at, target } => {
                self.foreground_target(target.as_ref())?;
                let detail = self.send_mouse_transition(session, at, *button, false)?;
                if let Some(state) = self.pressed.get_mut(session) {
                    state.mouse_buttons.remove(button);
                }
                (detail, "mouse_up")
            }
            ComputerAction::MiddleClick { at, target } => {
                self.foreground_target(target.as_ref())?;
                (
                    self.send_mouse_button(session, at, MouseButton::Middle, 1)?,
                    "middle_click",
                )
            }
            ComputerAction::TripleClick { at, target } => {
                self.foreground_target(target.as_ref())?;
                (
                    self.send_mouse_button(session, at, MouseButton::Left, 3)?,
                    "triple_click",
                )
            }
            ComputerAction::ModifierClick {
                modifier,
                button,
                at,
                target,
            } => {
                self.foreground_target(target.as_ref())?;
                (
                    self.send_modifier_click(session, at, modifier, *button)?,
                    "modifier_click",
                )
            }
            ComputerAction::KeyDown { key, target } => {
                self.foreground_target(target.as_ref())?;
                let (_, detail) = self.send_key_transition(key, true)?;
                self.pressed_state_mut(session).keys.insert(state_key(key));
                (detail, "key_down")
            }
            ComputerAction::KeyUp { key, target } => {
                self.foreground_target(target.as_ref())?;
                let (_, detail) = self.send_key_transition(key, false)?;
                if let Some(state) = self.pressed.get_mut(session) {
                    state.keys.remove(&state_key(key));
                }
                (detail, "key_up")
            }
            ComputerAction::HoldKey {
                key,
                duration_ms,
                target,
            } => {
                self.foreground_target(target.as_ref())?;
                if *duration_ms == 0 || *duration_ms > MAX_HOLD_KEY_MS {
                    return Err(ComputerError::InvalidAction(format!(
                        "HoldKey duration must be between 1 and {MAX_HOLD_KEY_MS} ms"
                    )));
                }
                let (_, down_detail) = self.send_key_transition(key, true)?;
                let normalized = state_key(key);
                self.pressed_state_mut(session)
                    .keys
                    .insert(normalized.clone());
                thread::sleep(Duration::from_millis(*duration_ms as u64));
                let up_result = self.send_key_transition(key, false);
                match up_result {
                    Ok((_, up_detail)) => {
                        if let Some(state) = self.pressed.get_mut(session) {
                            state.keys.remove(&normalized);
                        }
                        (
                            format!("{down_detail}; {up_detail}; duration_ms={duration_ms}"),
                            "hold_key",
                        )
                    }
                    Err(error) => {
                        self.release_pressed(session);
                        return Err(error);
                    }
                }
            }
        };
        Ok(ComputerActionResult {
            status: ActionStatus::Performed,
            observation: None,
            backend_detail: Some(format!(
                "backend=win-native; action={action_name}; {detail}"
            )),
        })
    }

    async fn semantic_observe(
        &mut self,
        session: &ComputerSessionId,
        window: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let display = self.primary_display_info()?;
        let hwnd = Self::hwnd(window)?;
        self.uia.observe(&display, hwnd, session, window, limits)
    }

    async fn capability_probe(
        &mut self,
        session: &ComputerSessionId,
        window_id: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let total_started = Instant::now();
        let display = self.primary_display_info()?;
        let topology_generation = display.topology.topology_generation;

        let window_started = Instant::now();
        let hwnd = match Self::hwnd(window_id) {
            Ok(hwnd) => hwnd,
            Err(error) => {
                self.capability_cache
                    .get_mut(session)
                    .map(|cache| cache.entries.remove(window_id));
                return Err(error);
            }
        };
        let window = match self
            .enumerate_window_records(&display)?
            .into_iter()
            .find(|candidate| candidate.id == *window_id)
        {
            Some(window) => window,
            None => {
                self.capability_cache
                    .get_mut(session)
                    .map(|cache| cache.entries.remove(window_id));
                return Err(ComputerError::InvalidWindow(format!(
                    "window {window_id} is not present in the current observation"
                )));
            }
        };
        let application = application_identity(&window, hwnd);
        let window_probe_ms = window_started.elapsed().as_millis();

        let security_started = Instant::now();
        let decision = self.security_decision_for_hwnd(hwnd, "capability_probe")?;
        let security_probe_ms = security_started.elapsed().as_millis();
        let security_key = security_cache_key(&decision);
        let semantic_before = self.uia.current_generation(session).unwrap_or_default();
        if let Some(mut profile) = self
            .capability_cache
            .get(session)
            .and_then(|cache| cache.entries.get(window_id))
            .filter(|entry| {
                entry.process_id == window.process_id
                    && entry.security_key == security_key
                    && entry.topology_generation == topology_generation
                    && entry.semantic_generation == semantic_before
            })
            .map(|entry| entry.profile.clone())
        {
            {
                profile.cache_hit = true;
                profile.timings = CapabilityProbeTimings::default();
                return Ok(profile);
            }
        }

        // Capability admission is a read-only probe. It must not call
        // UiaStore::observe here: that would advance the authoritative
        // semantic generation and invalidate the element reference that the
        // caller just supplied. When a current snapshot is available, infer
        // capabilities from it; otherwise keep semantic capability unknown.
        let uia_probe_started = Instant::now();
        let uia_snapshot = self.uia.current_elements_for_window(
            session,
            window_id,
            SemanticObservationLimits {
                max_depth: 2,
                max_elements: 64,
            },
        );
        let uia_probe_ms = uia_probe_started.elapsed().as_millis();
        let (semantic, semantic_actions, semantic_generation, semantic_detail) = match uia_snapshot
        {
            Some((elements, generation)) => {
                let any = |predicate: fn(&alice_computer_use_core::ComputerElement) -> bool| {
                    elements.iter().any(predicate)
                };
                let action = |supported: bool, name: &str| {
                    if supported {
                        CapabilityAssessment::supported(
                            CapabilitySource::ProviderReported,
                            format!("current UIA observation found {name}"),
                        )
                    } else {
                        CapabilityAssessment::unsupported(
                            CapabilitySource::ProviderReported,
                            format!("current UIA observation found no {name} pattern"),
                        )
                    }
                };
                (
                    CapabilityAssessment::supported(
                        CapabilitySource::Probed,
                        format!("current UIA observation found {} elements", elements.len()),
                    ),
                    CapabilitySemanticActionProfile {
                        focus: action(any(|element| element.focusable), "focus"),
                        invoke: action(any(|element| element.capabilities.invokable), "invoke"),
                        set_value: action(
                            any(|element| element.capabilities.editable),
                            "set_value",
                        ),
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
                        scroll_into_view: action(
                            any(|element| element.capabilities.scroll_into_view),
                            "scroll_into_view",
                        ),
                    },
                    Some(generation),
                    None,
                )
            }
            None => (
                CapabilityAssessment::unknown(
                    CapabilitySource::Probed,
                    "no current semantic observation is available; probe did not refresh UIA",
                ),
                unknown_semantic_actions("no current semantic observation"),
                None,
                Some("no current semantic observation; probe did not refresh UIA".into()),
            ),
        };

        let boundary_reason = decision.reason.clone();
        let mut profile = ApplicationCapabilityProfile {
            window_id: window_id.clone(),
            framework_hint: classify_framework(
                application.top_level_window_class.as_deref(),
                application.executable_name.as_deref(),
            ),
            application,
            observation: CapabilityObservationProfile {
                window: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "live top-level window enumeration",
                ),
                pixel: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "primary display geometry and native capture path are available",
                ),
                semantic,
                frame: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "primary display frame metadata is available",
                ),
            },
            input: CapabilityInputProfile {
                pointer: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "WinNative pointer primitives are available",
                ),
                keyboard: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "WinNative physical-key primitives are available",
                ),
                text: CapabilityAssessment::supported(
                    CapabilitySource::Probed,
                    "WinNative Unicode text primitives are available",
                ),
            },
            execution: CapabilityExecutionProfile {
                semantic_preferred: if semantic_actions_supported(&semantic_actions) {
                    CapabilityAssessment::supported(
                        CapabilitySource::ProviderReported,
                        "at least one semantic action is admitted by the bounded provider probe",
                    )
                } else if semantic_generation.is_some() {
                    CapabilityAssessment::unsupported(
                        CapabilitySource::ProviderReported,
                        "bounded provider probe found no semantic action pattern",
                    )
                } else {
                    CapabilityAssessment::unknown(
                        CapabilitySource::Probed,
                        "semantic provider could not be probed",
                    )
                },
                pixel_fallback_available: CapabilityAssessment::supported(
                    CapabilitySource::Static,
                    "R3 permits explicit pixel fallback when independently admitted",
                ),
            },
            semantic_actions,
            environment: CapabilityEnvironmentProfile {
                access_boundary: decision.boundary,
                display_compatibility: CapabilityAssessment::supported(
                    CapabilitySource::Observed,
                    "primary display physical/logical geometry is available",
                ),
                topology_compatibility: if display.topology.displays.len() > 1 {
                    CapabilityAssessment::restricted(
                        CapabilitySource::Static,
                        "production coordinate/capture path is primary-monitor-only",
                    )
                } else {
                    CapabilityAssessment::supported(
                        CapabilitySource::Observed,
                        "single primary display topology",
                    )
                },
            },
            restrictions: CapabilityRestrictions {
                elevation_required: matches!(
                    decision.boundary,
                    ComputerAccessBoundary::ElevationRequired
                ),
                foreground_required: true,
                reasons: Vec::new(),
            },
            security: window.security.clone(),
            semantic_generation,
            cache_hit: false,
            timings: CapabilityProbeTimings {
                window_probe_ms,
                uia_probe_ms,
                security_probe_ms,
                total_ms: total_started.elapsed().as_millis(),
            },
            known_gaps: Vec::new(),
        };
        if display.topology.displays.len() > 1 {
            profile
                .known_gaps
                .push("multi-monitor compatibility is environment-deferred".into());
        }
        if let Some(detail) = semantic_detail {
            profile.known_gaps.push(detail);
        } else if !semantic_actions_supported(&profile.semantic_actions) {
            profile
                .known_gaps
                .push("no semantic action pattern was found in the bounded sample".into());
        }
        if decision.boundary != ComputerAccessBoundary::Allowed {
            profile.apply_security_restriction(boundary_reason.clone());
            if !matches!(
                decision.capabilities.semantic_observation,
                CapabilityAccess::Allowed
            ) {
                profile.observation.semantic = match decision.capabilities.semantic_observation {
                    CapabilityAccess::Denied { .. } => CapabilityAssessment::restricted(
                        CapabilitySource::SecurityPolicy,
                        boundary_reason.clone(),
                    ),
                    CapabilityAccess::Unknown { .. } => CapabilityAssessment::unknown(
                        CapabilitySource::SecurityPolicy,
                        boundary_reason.clone(),
                    ),
                    CapabilityAccess::Allowed => profile.observation.semantic.clone(),
                };
            }
        }

        let cache = self.capability_cache.entry(session.clone()).or_default();
        if cache.entries.len() >= CAPABILITY_CACHE_MAX {
            if let Some(oldest) = cache.entries.keys().next().cloned() {
                cache.entries.remove(&oldest);
            }
        }
        let generation = profile.semantic_generation.unwrap_or(semantic_before);
        cache.entries.insert(
            window_id.clone(),
            CapabilityCacheEntry {
                process_id: window.process_id,
                security_key,
                topology_generation,
                semantic_generation: generation,
                profile: profile.clone(),
            },
        );
        Ok(profile)
    }

    async fn validate_element(
        &mut self,
        session: &ComputerSessionId,
        element: &ElementId,
    ) -> Result<(), ComputerError> {
        self.ensure_initialized()?;
        self.uia.validate(session, element)
    }

    async fn resolve_element_window(
        &mut self,
        session: &ComputerSessionId,
        element: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        self.ensure_initialized()?;
        self.uia.element_window(session, element)
    }

    async fn semantic_action(
        &mut self,
        session: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let display = self.primary_display_info()?;
        let window_id = self.uia.element_window(session, action.element_id())?;
        let decision = self.security_decision_for_window_id(&window_id, "semantic_action")?;
        Self::admit_security(&decision)?;
        let mut result = self.uia.action(&display, session, action)?;
        result.security = Some(decision);
        Ok(result)
    }

    async fn execute_policy(
        &mut self,
        session: &ComputerSessionId,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, ComputerError> {
        self.ensure_initialized()?;
        self.refresh_topology()?;
        let total_started = Instant::now();
        let display = self.primary_display_info()?;
        let mut security = None;
        match &request.intent {
            ComputerExecutionIntent::Pixel {
                target_window_id: Some(window_id),
                ..
            } => match self.security_decision_for_window_id(window_id, "pixel_execution") {
                Ok(decision) => {
                    if let Err(error) = Self::admit_security(&decision) {
                        return Ok(security_rejected_execution_result(
                            request,
                            Some(decision),
                            error,
                        ));
                    }
                    security = Some(decision);
                }
                Err(error) if is_security_error(&error) => {
                    return Ok(security_rejected_execution_result(request, None, error));
                }
                Err(error) => return Err(error),
            },
            ComputerExecutionIntent::Semantic(action) => {
                if let Ok(window_id) = self.uia.element_window(session, action.element_id()) {
                    match self.security_decision_for_window_id(&window_id, "semantic_execution") {
                        Ok(decision) => {
                            if let Err(error) = Self::admit_security(&decision) {
                                return Ok(security_rejected_execution_result(
                                    request,
                                    Some(decision),
                                    error,
                                ));
                            }
                            security = Some(decision);
                        }
                        Err(error) if is_security_error(&error) => {
                            return Ok(security_rejected_execution_result(request, None, error));
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
            ComputerExecutionIntent::Pixel {
                target_window_id: None,
                ..
            } => {}
        }
        let mut attempts = Vec::new();
        let mut semantic_element_id = request.semantic_element_id().cloned();
        let mut pixel_target = None;
        let mut verification = ComputerExecutionVerification::default();
        let mut generation_before = None;
        let mut generation_after = None;
        let mut fallback_used = false;
        let mut final_outcome = ComputerExecutionOutcome::InvalidRequest;
        let mut explanation: Option<String>;
        let mut semantic_attempt_ms = 0;
        let mut fallback_decision_ms = 0;
        let mut pixel_attempt_ms = 0;
        let mut verification_ms = 0;
        let policy_started = Instant::now();

        let mut pixel_action = None;
        let mut pixel_window_id = None;
        let mut before_snapshot = None;

        match &request.intent {
            ComputerExecutionIntent::Pixel {
                action,
                target_window_id,
            } => {
                if request.strategy == ComputerExecutionStrategy::SemanticOnly {
                    final_outcome = ComputerExecutionOutcome::InvalidRequest;
                    explanation =
                        Some("SemanticOnly cannot execute a Pixel execution intent".into());
                } else if target_window_id.is_none() {
                    final_outcome = ComputerExecutionOutcome::InvalidRequest;
                    explanation = Some(
                        "pixel execution requires an explicit containing target window".into(),
                    );
                } else {
                    pixel_action = Some(action.clone());
                    pixel_window_id = target_window_id.clone();
                    before_snapshot = self
                        .uia
                        .current_snapshot(session)
                        .ok()
                        .map(|(snapshot, _)| snapshot);
                    explanation =
                        Some("Pixel strategy was explicitly selected by the caller".into());
                }
            }
            ComputerExecutionIntent::Semantic(action) => {
                let semantic_method = execution_method_for_semantic(action);
                let semantic_allowed = matches!(
                    request.strategy,
                    ComputerExecutionStrategy::SemanticOnly
                        | ComputerExecutionStrategy::PreferSemantic
                );
                if semantic_allowed {
                    let cached_unsupported = self
                        .cached_semantic_assessment(session, action, security.as_ref())
                        .filter(|assessment| assessment.status == CapabilityStatus::Unsupported);
                    let semantic_started = Instant::now();
                    let semantic_result = if let Some(assessment) = cached_unsupported {
                        Ok(SemanticActionResult {
                            action: action.clone(),
                            element_id: action.element_id().clone(),
                            status: SemanticActionStatus::Unsupported,
                            verification: alice_computer_use_core::SemanticActionVerification {
                                detail: assessment.provenance.detail.clone(),
                                ..Default::default()
                            },
                            timing: alice_computer_use_core::SemanticActionTiming::default(),
                            observation_generation_before: self.uia.current_generation(session),
                            observation_generation_after: None,
                            security: security.clone(),
                        })
                    } else {
                        self.uia.action(&display, session, action)
                    };
                    semantic_attempt_ms = semantic_started.elapsed().as_millis();
                    match semantic_result {
                        Ok(result) => {
                            semantic_element_id = Some(result.element_id.clone());
                            generation_before = result.observation_generation_before;
                            generation_after = result.observation_generation_after;
                            verification = execution_verification_from_semantic(action, &result);
                            verification_ms = millis_from_micros(result.timing.verification_micros);
                            let outcome = execution_outcome_from_semantic(result.status);
                            attempts.push(ComputerExecutionAttempt {
                                method: semantic_method,
                                outcome,
                                semantic_element_id: Some(result.element_id.clone()),
                                pixel_target: None,
                                verification: verification.clone(),
                                generation_before,
                                generation_after,
                                timing_ms: millis_from_micros(result.timing.total_micros),
                                detail: verification.detail.clone(),
                            });

                            if result.status == SemanticActionStatus::Performed {
                                final_outcome = ComputerExecutionOutcome::Performed;
                                explanation =
                                    Some("semantic action succeeded and was verified".into());
                            } else if result.status == SemanticActionStatus::OutcomeUnknown
                                || result.status == SemanticActionStatus::VerificationFailed
                                || !semantic_failure_can_fallback(result.status)
                            {
                                final_outcome = outcome;
                                explanation = Some(format!(
                                    "semantic action stopped with {:?}; pixel fallback is prohibited",
                                    result.status
                                ));
                            } else if request.strategy == ComputerExecutionStrategy::SemanticOnly {
                                final_outcome = outcome;
                                explanation = Some(
                                    "SemanticOnly stopped after semantic capability failure".into(),
                                );
                            } else if request.fallback_policy != ComputerFallbackPolicy::Allow
                                || !action_allows_pixel_fallback(action)
                            {
                                final_outcome = ComputerExecutionOutcome::FallbackDenied;
                                explanation = Some(format!(
                                    "semantic {:?} was unsupported but fallback policy is {:?}",
                                    action, request.fallback_policy
                                ));
                            } else {
                                let decision_started = Instant::now();
                                match self.prepare_pixel_fallback(session, &display, action).await {
                                    Ok((target, click, snapshot)) => {
                                        pixel_target = Some(target.clone());
                                        pixel_window_id = Some(target.window_id.clone());
                                        pixel_action = Some(click);
                                        before_snapshot = Some(snapshot);
                                        fallback_used = true;
                                        explanation = Some(
                                            "semantic capability failure admitted one validated pixel fallback".into(),
                                        );
                                    }
                                    Err(error) => {
                                        final_outcome = execution_outcome_from_error(&error);
                                        explanation = Some(format!(
                                            "pixel fallback admission failed after semantic {:?}: {error}",
                                            result.status
                                        ));
                                    }
                                }
                                fallback_decision_ms = decision_started.elapsed().as_millis();
                            }
                        }
                        Err(error) => {
                            let outcome = execution_outcome_from_error(&error);
                            let attempt_verification = ComputerExecutionVerification {
                                detail: Some(error.to_string()),
                                ..Default::default()
                            };
                            attempts.push(ComputerExecutionAttempt {
                                method: semantic_method,
                                outcome,
                                semantic_element_id: semantic_element_id.clone(),
                                pixel_target: None,
                                verification: attempt_verification.clone(),
                                generation_before: None,
                                generation_after: None,
                                timing_ms: semantic_attempt_ms,
                                detail: attempt_verification.detail.clone(),
                            });
                            verification = attempt_verification;
                            if request.strategy == ComputerExecutionStrategy::PreferSemantic
                                && request.fallback_policy == ComputerFallbackPolicy::Allow
                                && action_allows_pixel_fallback(action)
                                && matches!(
                                    error,
                                    ComputerError::Unsupported { .. }
                                        | ComputerError::CapabilityGap { .. }
                                )
                            {
                                let decision_started = Instant::now();
                                match self.prepare_pixel_fallback(session, &display, action).await {
                                    Ok((target, click, snapshot)) => {
                                        pixel_target = Some(target.clone());
                                        pixel_window_id = Some(target.window_id.clone());
                                        pixel_action = Some(click);
                                        before_snapshot = Some(snapshot);
                                        fallback_used = true;
                                        explanation = Some(
                                            "provider capability error admitted one validated pixel fallback".into(),
                                        );
                                    }
                                    Err(fallback_error) => {
                                        final_outcome =
                                            execution_outcome_from_error(&fallback_error);
                                        explanation = Some(format!(
                                            "provider capability error and fallback admission failed: {fallback_error}"
                                        ));
                                    }
                                }
                                fallback_decision_ms = decision_started.elapsed().as_millis();
                            } else {
                                final_outcome = outcome;
                                explanation = Some(format!(
                                    "semantic provider error stopped execution: {error}"
                                ));
                            }
                        }
                    }
                } else {
                    let decision_started = Instant::now();
                    match self.prepare_pixel_fallback(session, &display, action).await {
                        Ok((target, click, snapshot)) => {
                            pixel_target = Some(target.clone());
                            pixel_window_id = Some(target.window_id.clone());
                            pixel_action = Some(click);
                            before_snapshot = Some(snapshot);
                            explanation =
                                Some("PreferPixel explicitly selected by the caller".into());
                        }
                        Err(error) => {
                            final_outcome = execution_outcome_from_error(&error);
                            explanation = Some(format!("explicit pixel admission failed: {error}"));
                        }
                    }
                    fallback_decision_ms = decision_started.elapsed().as_millis();
                }
            }
        }

        if let (Some(action), Some(window_id)) = (pixel_action, pixel_window_id) {
            let pixel_started = Instant::now();
            let pixel_result = self
                .run_pixel_attempt(
                    session,
                    &display,
                    &window_id,
                    &action,
                    before_snapshot.as_deref(),
                )
                .await;
            pixel_attempt_ms = pixel_started.elapsed().as_millis();
            verification_ms = verification_ms.max(pixel_result.verification_ms);
            final_outcome = pixel_result.outcome;
            generation_after = pixel_result.generation_after;
            verification = pixel_result.verification.clone();
            attempts.push(ComputerExecutionAttempt {
                method: if matches!(action, ComputerAction::Click { .. }) {
                    ComputerExecutionMethod::PixelClick
                } else {
                    ComputerExecutionMethod::PixelAction
                },
                outcome: pixel_result.outcome,
                semantic_element_id: semantic_element_id.clone(),
                pixel_target: pixel_target.clone(),
                verification: pixel_result.verification,
                generation_before,
                generation_after,
                timing_ms: pixel_attempt_ms,
                detail: pixel_result.detail,
            });
        }

        if attempts.is_empty() && final_outcome == ComputerExecutionOutcome::InvalidRequest {
            explanation = Some("execution request was rejected before any attempt".into());
        }
        let policy_ms = policy_started.elapsed().as_millis();
        Ok(ComputerExecutionResult {
            requested_intent: request.intent.clone(),
            selected_strategy: request.strategy,
            attempts,
            final_outcome,
            semantic_element_id,
            pixel_target,
            verification,
            fallback_used,
            generation_before,
            generation_after,
            timing: ComputerExecutionTiming {
                policy_ms,
                semantic_attempt_ms,
                fallback_decision_ms,
                pixel_attempt_ms,
                verification_ms,
                total_ms: total_started.elapsed().as_millis(),
            },
            explanation: explanation.unwrap_or_else(|| "execution policy completed".into()),
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
        self.uia.close_session(session);
        self.frames.remove(session);
        self.capability_cache.remove(session);
        Ok(())
    }
}

fn unknown_semantic_actions(detail: &str) -> CapabilitySemanticActionProfile {
    let unknown = || CapabilityAssessment::unknown(CapabilitySource::Probed, detail);
    CapabilitySemanticActionProfile {
        focus: unknown(),
        invoke: unknown(),
        set_value: unknown(),
        toggle: unknown(),
        select: unknown(),
        expand_collapse: unknown(),
        range_value: unknown(),
        scroll_into_view: unknown(),
    }
}

fn semantic_actions_supported(profile: &CapabilitySemanticActionProfile) -> bool {
    [
        &profile.focus,
        &profile.invoke,
        &profile.set_value,
        &profile.toggle,
        &profile.select,
        &profile.expand_collapse,
        &profile.range_value,
        &profile.scroll_into_view,
    ]
    .iter()
    .any(|capability| capability.status == CapabilityStatus::Supported)
}

fn security_cache_key(decision: &SecurityDecision) -> String {
    format!(
        "boundary={:?};source={:?};target={:?};caps={:?}",
        decision.boundary, decision.source, decision.target, decision.capabilities
    )
}

fn application_identity(window: &Window, hwnd: HWND) -> ApplicationIdentity {
    let executable_path = window
        .process_id
        .and_then(process_image_path)
        .filter(|path| !path.is_empty());
    let executable_name = executable_path.as_deref().and_then(|path| {
        std::path::Path::new(path)
            .file_name()
            .map(|value| value.to_string_lossy().into_owned())
    });
    let top_level_window_class = window_class_name(hwnd);
    let framework_hint = classify_framework(
        top_level_window_class.as_deref(),
        executable_name.as_deref(),
    );
    ApplicationIdentity {
        process_id: window.process_id,
        executable_name,
        executable_path,
        executable_hash: None,
        process_architecture: ProcessArchitecture::Unknown,
        top_level_window_class,
        framework_hints: vec![framework_hint],
        version: None,
    }
}

fn window_class_name(hwnd: HWND) -> Option<String> {
    let mut buffer = [0u16; 256];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    (length > 0).then(|| String::from_utf16_lossy(&buffer[..length as usize]))
}

fn process_image_path(process_id: u32) -> Option<String> {
    let process =
        unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, process_id) }.ok()?;
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    result.ok()?;
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn classify_framework(class_name: Option<&str>, executable_name: Option<&str>) -> FrameworkHint {
    let class = class_name.unwrap_or_default().to_ascii_lowercase();
    let executable = executable_name.unwrap_or_default().to_ascii_lowercase();
    if executable.contains("msedgewebview2") {
        FrameworkHint::WebView2
    } else if executable.contains("electron")
        || executable.contains("code")
        || class.contains("chrome_widgetwin")
    {
        FrameworkHint::ChromiumElectron
    } else if class.contains("applicationframewindow") {
        FrameworkHint::UwpXaml
    } else if class.contains("hwndwrapper") {
        FrameworkHint::Wpf
    } else if class.contains("notepad") {
        FrameworkHint::WinUi
    } else if class.is_empty() {
        FrameworkHint::Unknown
    } else {
        FrameworkHint::Win32
    }
}

fn millis_from_micros(value: u128) -> u128 {
    value / 1_000
}

fn external_input_action_requires_fixture_evidence(action: &ComputerAction) -> bool {
    matches!(
        action,
        ComputerAction::MouseDown { .. }
            | ComputerAction::MouseUp { .. }
            | ComputerAction::MiddleClick { .. }
            | ComputerAction::TripleClick { .. }
            | ComputerAction::ModifierClick { .. }
            | ComputerAction::KeyDown { .. }
            | ComputerAction::KeyUp { .. }
            | ComputerAction::HoldKey { .. }
    )
}

fn semantic_snapshot_is_opaque(elements: &[alice_computer_use_core::ComputerElement]) -> bool {
    !elements.is_empty()
        && elements.len() <= 4
        && elements.iter().all(|element| {
            !element.capabilities.invokable
                && !element.capabilities.editable
                && !element.capabilities.toggleable
                && !element.capabilities.selectable
                && !element.capabilities.expandable
                && !element.capabilities.range_adjustable
                && !element.capabilities.scroll_into_view
                && matches!(element.control_type.as_str(), "window" | "pane" | "custom")
        })
}

fn execution_method_for_semantic(action: &SemanticAction) -> ComputerExecutionMethod {
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

fn execution_outcome_from_error(error: &ComputerError) -> ComputerExecutionOutcome {
    match error {
        ComputerError::InvalidAction(_) => ComputerExecutionOutcome::InvalidRequest,
        ComputerError::InvalidWindow(_) => ComputerExecutionOutcome::InvalidTarget,
        ComputerError::UnknownDisplay(_) | ComputerError::StaleDisplay(_) => {
            ComputerExecutionOutcome::InvalidTarget
        }
        ComputerError::StaleElement(_) => ComputerExecutionOutcome::StaleElement,
        ComputerError::UnknownElement(_) => ComputerExecutionOutcome::UnknownElement,
        ComputerError::InvalidCoordinate(_) => ComputerExecutionOutcome::InvalidTarget,
        ComputerError::SessionClosed => ComputerExecutionOutcome::UnknownElement,
        ComputerError::NotInitialized | ComputerError::AlreadyInitialized => {
            ComputerExecutionOutcome::BackendError
        }
        ComputerError::ForegroundDenied(_) | ComputerError::FocusNotAcquired(_) => {
            ComputerExecutionOutcome::FocusDenied
        }
        ComputerError::Unsupported { .. } | ComputerError::CapabilityGap { .. } => {
            ComputerExecutionOutcome::Unsupported
        }
        ComputerError::IntegrityMismatch(_) => ComputerExecutionOutcome::IntegrityMismatch,
        ComputerError::ElevationRequired(_) => ComputerExecutionOutcome::ElevationRequired,
        ComputerError::UipiDenied(_) => ComputerExecutionOutcome::UipiDenied,
        ComputerError::ProtectedDesktop(_) => ComputerExecutionOutcome::ProtectedDesktop,
        ComputerError::SecurityContextUnavailable(_) => {
            ComputerExecutionOutcome::SecurityContextUnavailable
        }
        ComputerError::AccessDenied(_) => ComputerExecutionOutcome::UipiDenied,
        ComputerError::TargetUnavailable(_) => ComputerExecutionOutcome::TargetUnavailable,
        ComputerError::Backend(_) => ComputerExecutionOutcome::BackendError,
    }
}

fn is_security_error(error: &ComputerError) -> bool {
    matches!(
        error,
        ComputerError::IntegrityMismatch(_)
            | ComputerError::ElevationRequired(_)
            | ComputerError::UipiDenied(_)
            | ComputerError::ProtectedDesktop(_)
            | ComputerError::SecurityContextUnavailable(_)
            | ComputerError::AccessDenied(_)
            | ComputerError::TargetUnavailable(_)
    )
}

fn security_rejected_execution_result(
    request: &ComputerExecutionRequest,
    security: Option<SecurityDecision>,
    error: ComputerError,
) -> ComputerExecutionResult {
    ComputerExecutionResult {
        requested_intent: request.intent.clone(),
        selected_strategy: request.strategy,
        attempts: Vec::new(),
        final_outcome: execution_outcome_from_error(&error),
        semantic_element_id: request.semantic_element_id().cloned(),
        pixel_target: None,
        verification: ComputerExecutionVerification {
            detail: Some(format!("security preflight denied side effects: {error}")),
            ..Default::default()
        },
        fallback_used: false,
        generation_before: None,
        generation_after: None,
        timing: ComputerExecutionTiming::default(),
        explanation: format!("security preflight denied execution: {error}"),
        security,
    }
}

fn execution_verification_from_semantic(
    action: &SemanticAction,
    result: &SemanticActionResult,
) -> ComputerExecutionVerification {
    let kind = match action {
        SemanticAction::Focus { .. } => ComputerExecutionVerificationKind::Focused,
        SemanticAction::Invoke { .. } => ComputerExecutionVerificationKind::SemanticStateChanged,
        SemanticAction::SetValue { .. } => ComputerExecutionVerificationKind::ValueEquals,
        SemanticAction::SetRangeValue { .. } => ComputerExecutionVerificationKind::ValueEquals,
        SemanticAction::Toggle { .. }
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

fn semantic_failure_can_fallback(status: SemanticActionStatus) -> bool {
    matches!(
        status,
        SemanticActionStatus::Unsupported | SemanticActionStatus::ElementUnavailable
    )
}

fn action_allows_pixel_fallback(action: &SemanticAction) -> bool {
    matches!(
        action,
        SemanticAction::Focus { .. }
            | SemanticAction::Invoke { .. }
            | SemanticAction::SetValue { .. }
    )
}

unsafe fn send_input(events: &[INPUT]) -> Result<(), ComputerError> {
    let mut marked_events = events.to_vec();
    let input_marker = configured_input_marker();
    for event in &mut marked_events {
        match event.r#type {
            INPUT_MOUSE => event.Anonymous.mi.dwExtraInfo = input_marker,
            INPUT_KEYBOARD => event.Anonymous.ki.dwExtraInfo = input_marker,
            _ => {}
        }
    }
    let sent = SendInput(&marked_events, size_of::<INPUT>() as i32);
    if sent as usize != events.len() {
        let inserted = (sent as usize).min(marked_events.len());
        if inserted > 0 {
            release_partial_input(&marked_events[..inserted]);
        }
        return Err(ComputerError::Backend(format!(
            "OUTCOME_UNKNOWN: SendInput inserted {} of {} events (win32_error={})",
            sent,
            events.len(),
            GetLastError().0
        )));
    }
    Ok(())
}

/// A short SendInput batch can be accepted only as a prefix.  Release every
/// key/button-down event from that prefix before reporting the outcome as
/// unknown, so a transport failure cannot leave the desktop held hostage.
unsafe fn release_partial_input(events: &[INPUT]) {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;

    let mut releases = Vec::new();
    for event in events.iter().rev() {
        match event.r#type {
            INPUT_KEYBOARD if event.Anonymous.ki.dwFlags.0 & KEYEVENTF_KEYUP.0 == 0 => {
                let mut release = *event;
                release.Anonymous.ki.dwFlags |= KEYEVENTF_KEYUP;
                releases.push(release);
            }
            INPUT_MOUSE => {
                let flags = event.Anonymous.mi.dwFlags.0;
                let up = if flags & MOUSEEVENTF_LEFTDOWN.0 != 0 {
                    Some(MOUSEEVENTF_LEFTUP)
                } else if flags & MOUSEEVENTF_RIGHTDOWN.0 != 0 {
                    Some(MOUSEEVENTF_RIGHTUP)
                } else if flags & MOUSEEVENTF_MIDDLEDOWN.0 != 0 {
                    Some(MOUSEEVENTF_MIDDLEUP)
                } else {
                    None
                };
                if let Some(up) = up {
                    let mut release = *event;
                    release.Anonymous.mi.dwFlags = up;
                    releases.push(release);
                }
            }
            _ => {}
        }
    }
    if !releases.is_empty() {
        let _ = SendInput(&releases, size_of::<INPUT>() as i32);
    }
}

/// Move the cursor through the same marked SendInput path as the action that
/// follows it. SetCursorPos cannot carry dwExtraInfo, so a low-level hook may
/// otherwise classify the cursor positioning step as an unrelated injector.
fn absolute_mouse_move_in_bounds(
    x: i32,
    y: i32,
    virtual_desktop_bounds: &Rect,
) -> Result<INPUT, ComputerError> {
    let left = virtual_desktop_bounds.origin.x.round() as i32;
    let top = virtual_desktop_bounds.origin.y.round() as i32;
    let width = virtual_desktop_bounds.size.width.round() as i32;
    let height = virtual_desktop_bounds.size.height.round() as i32;
    if width <= 1 || height <= 1 {
        return Err(ComputerError::Backend(
            "virtual desktop has invalid dimensions".into(),
        ));
    }

    Ok(INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: normalize_absolute_axis(x, left, width),
                dy: normalize_absolute_axis(y, top, height),
                mouseData: 0,
                dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    })
}

fn normalize_absolute_axis(value: i32, origin: i32, span: i32) -> i32 {
    let max = i64::from(span - 1);
    let offset = (i64::from(value) - i64::from(origin)).clamp(0, max);
    ((offset * 65_535) / max) as i32
}

unsafe fn release_modifiers(modifiers: &[VIRTUAL_KEY]) {
    let events: Vec<INPUT> = modifiers
        .iter()
        .rev()
        .map(|modifier| virtual_key_input(*modifier, true))
        .collect();
    if !events.is_empty() {
        let _ = send_input(&events);
    }
}

fn mouse_input(flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS) -> INPUT {
    mouse_input_with_data(flags, 0)
}

fn mouse_input_with_data(
    flags: windows::Win32::UI::Input::KeyboardAndMouse::MOUSE_EVENT_FLAGS,
    mouse_data: u32,
) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: mouse_data,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn unicode_input(unit: u16, up: bool) -> INPUT {
    let mut flags = KEYEVENTF_UNICODE;
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let scan = unsafe { MapVirtualKeyW(vk.0 as u32, MAPVK_VK_TO_VSC) as u16 };
    let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
    if scan != 0 {
        flags |= KEYEVENTF_SCANCODE;
    }
    if is_extended(vk) {
        flags |= KEYEVENTF_EXTENDEDKEY;
    }
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: if scan == 0 { vk } else { VIRTUAL_KEY(0) },
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn virtual_key_input(vk: VIRTUAL_KEY, up: bool) -> INPUT {
    let mut flags: KEYBD_EVENT_FLAGS = KEYBD_EVENT_FLAGS(0);
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: vk,
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

fn is_extended(vk: VIRTUAL_KEY) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    matches!(
        vk,
        VK_DELETE
            | VK_INSERT
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_UP
            | VK_DOWN
            | VK_LEFT
            | VK_RIGHT
            | VK_RCONTROL
            | VK_RMENU
            | VK_RWIN
            | VK_NUMLOCK
            | VK_SNAPSHOT
    )
}

fn modifier_vk(key: &str) -> Option<VIRTUAL_KEY> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    match key.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(VK_LCONTROL),
        "shift" => Some(VK_LSHIFT),
        "alt" | "menu" | "option" => Some(VK_LMENU),
        "win" | "meta" | "windows" | "cmd" | "command" => Some(VK_LWIN),
        _ => None,
    }
}

fn state_key(key: &str) -> String {
    key.trim().to_ascii_lowercase()
}

fn physical_vk(key: &str) -> Result<VIRTUAL_KEY, ComputerError> {
    modifier_vk(key).or_else(|| vk(key).ok()).ok_or_else(|| {
        ComputerError::InvalidAction(format!("unsupported WinNative physical key name: {key}"))
    })
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod frame_store_tests {
    use super::*;

    fn semantic_element(control_type: &str) -> alice_computer_use_core::ComputerElement {
        alice_computer_use_core::ComputerElement {
            id: ElementId::new(control_type),
            parent_id: None,
            child_ids: Vec::new(),
            role: control_type.into(),
            control_type: control_type.into(),
            name: None,
            automation_id: None,
            class_name: None,
            value_summary: None,
            text_summary: None,
            bounds: None,
            enabled: true,
            focused: false,
            focusable: true,
            offscreen: false,
            toggle_state: None,
            selected: None,
            expanded: None,
            range_value: None,
            range_minimum: None,
            range_maximum: None,
            capabilities: Default::default(),
        }
    }

    fn frame(id: &str, generation: u64, pixels: Vec<u8>) -> StoredFrame {
        StoredFrame {
            metadata: CaptureFrameMetadata {
                frame_id: FrameId::new(id),
                display_id: DisplayId::new("display-1-0"),
                topology_generation: generation,
                coordinate_space: CoordinateSpace::ScreenshotPixel,
                desktop_origin: Point { x: 0.0, y: 0.0 },
                width: 1,
                height: 1,
                pixel_format: FramePixelFormat::Bgra8,
                stride: 4,
                dpi: DpiScale::ONE,
                scale: DpiScale::ONE,
                captured_at: None,
                content_revision: generation,
                stale_topology: false,
            },
            pixels,
            encoded_png: None,
        }
    }

    #[test]
    fn root_and_pane_only_semantic_snapshot_is_opaque() {
        let mut root = semantic_element("window");
        let pane = semantic_element("pane");
        assert!(semantic_snapshot_is_opaque(&[root.clone(), pane]));
        root.capabilities.editable = true;
        assert!(!semantic_snapshot_is_opaque(&[root]));
        assert!(!semantic_snapshot_is_opaque(&[]));
    }

    #[test]
    fn unsupported_set_value_can_fallback_to_window_keyboard_input() {
        assert!(action_allows_pixel_fallback(&SemanticAction::SetValue {
            element_id: ElementId::new("opaque-root"),
            value: "hello".into(),
        }));
        assert!(!action_allows_pixel_fallback(&SemanticAction::Toggle {
            element_id: ElementId::new("toggle"),
        }));
    }

    #[test]
    fn absolute_input_uses_physical_capture_span_at_fractional_dpi() {
        // A 2560x1600 monitor at 150% has a 1707x1067 logical desktop. The
        // screenshot coordinate must be normalized against 2560x1600; using
        // the logical height would clamp this ordinary composer click to the
        // bottom edge (65535).
        let normalized_x = normalize_absolute_axis(400, 0, 2560);
        let normalized_y = normalize_absolute_axis(1083, 0, 1600);
        assert_eq!(normalized_x, 10_243);
        assert_eq!(normalized_y, 44_386);
        assert!(normalized_y < 65_535);
        assert_eq!(normalize_absolute_axis(1083, 0, 1067), 65_535);

        assert_eq!(normalize_absolute_axis(-1920, -1920, 1920), 0);
        assert_eq!(normalize_absolute_axis(-1, -1920, 1920), 65_535);
    }

    #[test]
    fn frame_store_is_bounded_and_eviction_is_deterministic() {
        let mut store = FrameStore {
            max_frames: 2,
            max_bytes: 8,
            ..Default::default()
        };
        store.insert(frame("a", 1, vec![0, 0, 0, 255])).unwrap();
        store.insert(frame("b", 1, vec![0, 0, 0, 255])).unwrap();
        store.insert(frame("c", 1, vec![0, 0, 0, 255])).unwrap();
        assert_eq!(
            store.state(&FrameId::new("a"), 1).state,
            FrameState::Evicted
        );
        assert_eq!(
            store.state(&FrameId::new("b"), 1).state,
            FrameState::Current
        );
        assert_eq!(
            store.state(&FrameId::new("c"), 1).state,
            FrameState::Current
        );
        assert_eq!(store.frames.len(), 2);
        assert!(store.total_bytes <= 8);
    }

    #[test]
    fn frame_store_reports_stale_reads_and_releases_memory() {
        let mut store = FrameStore::default();
        let id = FrameId::new("stale");
        store
            .insert(frame(id.as_str(), 1, vec![0, 0, 0, 255]))
            .unwrap();
        let stale = store.state(&id, 2);
        assert_eq!(stale.state, FrameState::StaleTopology);
        assert!(stale
            .metadata
            .as_ref()
            .is_some_and(|value| value.stale_topology));
        let released = store.release(&id);
        assert_eq!(released.state, FrameState::Released);
        assert_eq!(store.total_bytes, 0);
        assert_eq!(store.state(&id, 2).state, FrameState::Released);
    }

    #[test]
    fn frame_store_png_cache_is_reused_for_same_frame() {
        let mut store = FrameStore::default();
        let id = FrameId::new("png");
        store
            .insert(frame(id.as_str(), 1, vec![0, 0, 255, 255]))
            .unwrap();
        let (first, first_hit) = store.encode(&id, 1).unwrap();
        let (second, second_hit) = store.encode(&id, 1).unwrap();
        assert!(!first_hit);
        assert!(second_hit);
        assert!(!first.bytes.is_empty());
        assert_eq!(first.bytes, second.bytes);
        assert_eq!(store.encode_cache_misses, 1);
        assert_eq!(store.encode_cache_hits, 1);
    }

    #[test]
    fn security_boundary_is_conservative_for_protected_and_elevated_targets() {
        let protected = capabilities_for_boundary(
            ComputerAccessBoundary::ProtectedDesktop,
            "protected desktop",
        );
        assert!(matches!(
            protected.window_focus,
            CapabilityAccess::Denied { .. }
        ));
        assert!(matches!(
            protected.pointer_input,
            CapabilityAccess::Denied { .. }
        ));
        assert!(matches!(
            protected.keyboard_input,
            CapabilityAccess::Denied { .. }
        ));
        assert!(matches!(
            protected.semantic_action,
            CapabilityAccess::Denied { .. }
        ));

        let elevated = capabilities_for_boundary(
            ComputerAccessBoundary::ElevationRequired,
            "target requires elevation",
        );
        assert!(matches!(
            elevated.pixel_observation,
            CapabilityAccess::Allowed
        ));
        assert!(matches!(
            elevated.keyboard_input,
            CapabilityAccess::Denied { .. }
        ));
        assert!(matches!(
            elevated.semantic_action,
            CapabilityAccess::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn capability_cache_is_session_scoped_and_cleared_on_close() {
        let mut backend = WinNativeBackend::new();
        let session = ComputerSessionId::new("r9-session");
        backend
            .capability_cache
            .insert(session.clone(), CapabilityCache::default());
        assert!(backend.capability_cache.contains_key(&session));
        backend.close_session(&session).await.unwrap();
        assert!(!backend.capability_cache.contains_key(&session));
    }
}

fn vk(key: &str) -> Result<VIRTUAL_KEY, ComputerError> {
    use windows::Win32::UI::Input::KeyboardAndMouse::*;
    let normalized = key.to_ascii_lowercase();
    let named = match normalized.as_str() {
        "enter" | "return" => Some(VK_RETURN),
        "tab" => Some(VK_TAB),
        "backspace" | "back" => Some(VK_BACK),
        "escape" | "esc" => Some(VK_ESCAPE),
        "space" => Some(VK_SPACE),
        "delete" | "del" => Some(VK_DELETE),
        "insert" | "ins" => Some(VK_INSERT),
        "home" => Some(VK_HOME),
        "end" => Some(VK_END),
        "pageup" | "prior" => Some(VK_PRIOR),
        "pagedown" | "next" => Some(VK_NEXT),
        "f1" => Some(VK_F1),
        "f2" => Some(VK_F2),
        "f3" => Some(VK_F3),
        "f4" => Some(VK_F4),
        "f5" => Some(VK_F5),
        "f6" => Some(VK_F6),
        "f7" => Some(VK_F7),
        "f8" => Some(VK_F8),
        "f9" => Some(VK_F9),
        "f10" => Some(VK_F10),
        "f11" => Some(VK_F11),
        "f12" => Some(VK_F12),
        "left" => Some(VK_LEFT),
        "right" => Some(VK_RIGHT),
        "up" => Some(VK_UP),
        "down" => Some(VK_DOWN),
        "ctrl" | "control" => Some(VK_CONTROL),
        "shift" => Some(VK_SHIFT),
        "alt" | "menu" => Some(VK_MENU),
        "win" | "windows" | "meta" => Some(VK_LWIN),
        _ => None,
    };
    if let Some(key) = named {
        return Ok(key);
    }
    if normalized.len() == 1 {
        let byte = normalized.as_bytes()[0];
        if byte.is_ascii_alphanumeric() {
            return Ok(VIRTUAL_KEY(byte.to_ascii_uppercase() as u16));
        }
        if let Some(ch) = key.chars().next() {
            let packed =
                unsafe { windows::Win32::UI::Input::KeyboardAndMouse::VkKeyScanW(ch as u16) };
            if packed != -1 {
                return Ok(VIRTUAL_KEY((packed as u16) & 0xff));
            }
        }
    }
    Err(ComputerError::InvalidAction(format!(
        "unsupported WinNative key name: {key}"
    )))
}
