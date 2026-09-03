//! Structured Computer Use runtime.
//!
//! The runtime accepts only explicit pixel/input actions and explicit
//! semantic observation/action requests. It has no model loop, browser/CDP
//! route, shell route, clipboard read route, OCR, or RuntimeEvent integration.

use alice_computer_use_core::{
    ApplicationCapabilityProfile, CaptureFrameMetadata, ComputerAction, ComputerActionResult,
    ComputerError, ComputerExecutionRequest, ComputerExecutionResult, ComputerObservation,
    ComputerSessionId, DesktopSecurityContext, DisplayTopology, ElementId, FrameEncoding,
    FrameEncodingResult, FrameMetadataResult, ProcessSecurityContext, Screen, ScreenId, Screenshot,
    ScreenshotTarget, SemanticAction, SemanticActionResult, SemanticObservation,
    SemanticObservationLimits, Window, WindowId,
};
use async_trait::async_trait;
use std::{
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilityState {
    Supported,
    Limited,
    Unsupported,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Capability {
    pub name: &'static str,
    pub state: CapabilityState,
    pub detail: &'static str,
}

#[async_trait]
pub trait ComputerBackend: Send + Sync + 'static {
    async fn initialize(&mut self) -> Result<(), ComputerError>;
    async fn shutdown(&mut self) -> Result<(), ComputerError>;
    fn capabilities(&self) -> Vec<Capability>;
    async fn security_context(&mut self) -> Result<ProcessSecurityContext, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "security_context".into(),
            detail: "backend does not expose process security diagnostics".into(),
        })
    }
    async fn desktop_security_context(&mut self) -> Result<DesktopSecurityContext, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "desktop_security_context".into(),
            detail: "backend does not expose desktop security diagnostics".into(),
        })
    }
    async fn enumerate_screens(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<Vec<Screen>, ComputerError>;
    async fn enumerate_windows(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<Vec<Window>, ComputerError>;
    async fn screenshot(
        &mut self,
        session: &ComputerSessionId,
        screen: &ScreenId,
    ) -> Result<Screenshot, ComputerError>;
    /// Capture raw BGRA8 pixels into a backend-owned session-scoped frame.
    /// Consumers must call `encode_frame` when image bytes are actually
    /// needed; observation itself must not imply PNG encoding.
    async fn capture_frame(
        &mut self,
        _session: &ComputerSessionId,
        _screen: &ScreenId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "capture_frame".into(),
            detail: "backend does not provide raw frame capture".into(),
        })
    }
    async fn frame_metadata(
        &mut self,
        _session: &ComputerSessionId,
        _frame_id: &alice_computer_use_core::FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "frame_metadata".into(),
            detail: "backend does not provide frame storage".into(),
        })
    }
    async fn encode_frame(
        &mut self,
        _session: &ComputerSessionId,
        _frame_id: &alice_computer_use_core::FrameId,
        _encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "frame_encoding".into(),
            detail: "backend does not provide on-demand frame encoding".into(),
        })
    }
    async fn release_frame(
        &mut self,
        _session: &ComputerSessionId,
        _frame_id: &alice_computer_use_core::FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "frame_release".into(),
            detail: "backend does not provide frame storage".into(),
        })
    }
    async fn display_topology(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<DisplayTopology, ComputerError> {
        let screens = self.enumerate_screens(session).await?;
        Ok(DisplayTopology::from_displays(screens, 0))
    }
    /// Gather a coherent observation. Native backends can override this to
    /// refresh display topology once and reuse it for windows and the frame;
    /// the default keeps compatibility for small/test backends.
    async fn observe(
        &mut self,
        session: &ComputerSessionId,
    ) -> Result<ComputerObservation, ComputerError> {
        let topology = self.display_topology(session).await?;
        let screens = topology.displays.clone();
        let windows = self.enumerate_windows(session).await?;
        let active_window = windows
            .iter()
            .find(|window| window.active)
            .map(|window| window.id.clone());
        let primary = screens
            .iter()
            .find(|screen| screen.primary)
            .map(|screen| screen.id.clone());
        let frame = match primary {
            Some(screen) => Some(self.capture_frame(session, &screen).await?),
            None => None,
        };
        Ok(ComputerObservation {
            session_id: session.clone(),
            screens,
            windows,
            active_window,
            screenshot: None,
            frame,
            display_topology: Some(topology),
        })
    }
    async fn screenshot_target(
        &mut self,
        session: &ComputerSessionId,
        target: &ScreenshotTarget,
    ) -> Result<Screenshot, ComputerError> {
        match target {
            ScreenshotTarget::Display(display_id) => self.screenshot(session, display_id).await,
            ScreenshotTarget::VirtualDesktop => Err(ComputerError::Unsupported {
                capability: "virtual_desktop_screenshot".into(),
                detail: "backend does not provide virtual desktop capture".into(),
            }),
        }
    }
    async fn execute(
        &mut self,
        session: &ComputerSessionId,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError>;
    async fn semantic_observe(
        &mut self,
        _session: &ComputerSessionId,
        _window: &WindowId,
        _limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "semantic_observation".into(),
            detail: "backend does not provide UI Automation semantic observation".into(),
        })
    }
    async fn validate_element(
        &mut self,
        _session: &ComputerSessionId,
        _element: &ElementId,
    ) -> Result<(), ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "semantic_element_identity".into(),
            detail: "backend does not provide semantic element identity".into(),
        })
    }
    async fn resolve_element_window(
        &mut self,
        _session: &ComputerSessionId,
        _element: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "semantic_element_window".into(),
            detail: "backend does not resolve semantic elements to windows".into(),
        })
    }
    async fn semantic_action(
        &mut self,
        _session: &ComputerSessionId,
        _action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "semantic_action".into(),
            detail: "backend does not provide UI Automation semantic actions".into(),
        })
    }
    async fn capability_probe(
        &mut self,
        _session: &ComputerSessionId,
        _window: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "capability_probe".into(),
            detail: "backend does not provide application capability probing".into(),
        })
    }
    async fn execute_policy(
        &mut self,
        _session: &ComputerSessionId,
        _request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, ComputerError> {
        Err(ComputerError::Unsupported {
            capability: "execution_policy".into(),
            detail: "backend does not provide semantic/pixel execution policy".into(),
        })
    }
    /// Best-effort release of any Alice-held input for a session while
    /// retaining the session itself.  Takeover cleanup must not require a
    /// session close/reopen cycle just to clear pressed state.
    async fn cleanup_pressed(&mut self, _session: &ComputerSessionId) -> Result<(), ComputerError> {
        Ok(())
    }
    async fn close_session(&mut self, _session: &ComputerSessionId) -> Result<(), ComputerError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct ComputerRuntime<B: ComputerBackend> {
    backend: Arc<Mutex<B>>,
    initialized: Arc<AtomicBool>,
    next_session: Arc<AtomicU64>,
}

impl<B: ComputerBackend> ComputerRuntime<B> {
    pub fn new(backend: B) -> Self {
        Self {
            backend: Arc::new(Mutex::new(backend)),
            initialized: Arc::new(AtomicBool::new(false)),
            next_session: Arc::new(AtomicU64::new(1)),
        }
    }

    pub async fn initialize(&self) -> Result<(), ComputerError> {
        if self.initialized.swap(true, Ordering::AcqRel) {
            return Err(ComputerError::AlreadyInitialized);
        }
        let result = self.backend.lock().await.initialize().await;
        if result.is_err() {
            self.initialized.store(false, Ordering::Release);
        }
        result
    }

    pub async fn shutdown(&self) -> Result<(), ComputerError> {
        if !self.initialized.swap(false, Ordering::AcqRel) {
            return Ok(());
        }
        self.backend.lock().await.shutdown().await
    }

    pub async fn capabilities(&self) -> Vec<Capability> {
        self.backend.lock().await.capabilities()
    }

    pub async fn security_context(&self) -> Result<ProcessSecurityContext, ComputerError> {
        self.backend.lock().await.security_context().await
    }

    pub async fn desktop_security_context(&self) -> Result<DesktopSecurityContext, ComputerError> {
        self.backend.lock().await.desktop_security_context().await
    }

    pub async fn start_session(&self) -> Result<ComputerSession<B>, ComputerError> {
        if !self.initialized.load(Ordering::Acquire) {
            return Err(ComputerError::NotInitialized);
        }
        let sequence = self.next_session.fetch_add(1, Ordering::Relaxed);
        let millis = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        Ok(ComputerSession {
            id: ComputerSessionId::new(format!("alice-computer-session-{millis}-{sequence}")),
            backend: Arc::clone(&self.backend),
            closed: Arc::new(AtomicBool::new(false)),
        })
    }
}

#[cfg(windows)]
impl ComputerRuntime<WinNativeBackend> {
    /// Native diagnostic probe used by the R7 harness.  It is intentionally
    /// outside the backend-neutral contract and exposes counters only, never
    /// HWNDs or desktop pixels.
    pub async fn native_frame_store_stats(
        &self,
        session: &ComputerSessionId,
    ) -> NativeFrameStoreStats {
        self.backend.lock().await.frame_store_stats(session)
    }

    /// Diagnostic-only desktop identity used by the R8 manual gate. It
    /// exposes names, never desktop handles, through the native runtime API.
    pub async fn native_desktop_identity(&self) -> Result<(String, String), ComputerError> {
        self.backend.lock().await.desktop_identity()
    }
}

#[cfg(target_os = "macos")]
impl ComputerRuntime<MacNativeBackend> {
    pub async fn native_frame_store_stats(
        &self,
        session: &ComputerSessionId,
    ) -> NativeFrameStoreStats {
        self.backend.lock().await.frame_store_stats(session)
    }

    pub async fn native_desktop_identity(&self) -> Result<(String, String), ComputerError> {
        self.backend.lock().await.desktop_identity()
    }
}

pub struct ComputerSession<B: ComputerBackend> {
    id: ComputerSessionId,
    backend: Arc<Mutex<B>>,
    closed: Arc<AtomicBool>,
}

impl<B: ComputerBackend> Clone for ComputerSession<B> {
    fn clone(&self) -> Self {
        Self {
            id: self.id.clone(),
            backend: Arc::clone(&self.backend),
            closed: Arc::clone(&self.closed),
        }
    }
}

impl<B: ComputerBackend> ComputerSession<B> {
    pub fn id(&self) -> &ComputerSessionId {
        &self.id
    }

    fn ensure_open(&self) -> Result<(), ComputerError> {
        if self.closed.load(Ordering::Acquire) {
            Err(ComputerError::SessionClosed)
        } else {
            Ok(())
        }
    }

    pub async fn observe(&self) -> Result<ComputerObservation, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.observe(&self.id).await
    }

    pub async fn enumerate_screens(&self) -> Result<Vec<Screen>, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.enumerate_screens(&self.id).await
    }

    pub async fn display_topology(&self) -> Result<DisplayTopology, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.display_topology(&self.id).await
    }

    pub async fn enumerate_windows(&self) -> Result<Vec<Window>, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.enumerate_windows(&self.id).await
    }

    pub async fn screenshot(&self, screen: &ScreenId) -> Result<Screenshot, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.screenshot(&self.id, screen).await
    }

    pub async fn capture_frame(
        &self,
        screen: &ScreenId,
    ) -> Result<CaptureFrameMetadata, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .capture_frame(&self.id, screen)
            .await
    }

    pub async fn frame_metadata(
        &self,
        frame_id: &alice_computer_use_core::FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .frame_metadata(&self.id, frame_id)
            .await
    }

    pub async fn encode_frame(
        &self,
        frame_id: &alice_computer_use_core::FrameId,
        encoding: FrameEncoding,
    ) -> Result<FrameEncodingResult, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .encode_frame(&self.id, frame_id, encoding)
            .await
    }

    pub async fn release_frame(
        &self,
        frame_id: &alice_computer_use_core::FrameId,
    ) -> Result<FrameMetadataResult, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .release_frame(&self.id, frame_id)
            .await
    }

    pub async fn screenshot_target(
        &self,
        target: &ScreenshotTarget,
    ) -> Result<Screenshot, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .screenshot_target(&self.id, target)
            .await
    }

    pub async fn execute(
        &self,
        action: &ComputerAction,
    ) -> Result<ComputerActionResult, ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.execute(&self.id, action).await
    }

    pub async fn semantic_observe(
        &self,
        window: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .semantic_observe(&self.id, window, limits)
            .await
    }

    pub async fn validate_element(&self, element: &ElementId) -> Result<(), ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .validate_element(&self.id, element)
            .await
    }

    pub async fn resolve_element_window(
        &self,
        element: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .resolve_element_window(&self.id, element)
            .await
    }

    pub async fn semantic_action(
        &self,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .semantic_action(&self.id, action)
            .await
    }

    pub async fn capability_probe(
        &self,
        window: &WindowId,
    ) -> Result<ApplicationCapabilityProfile, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .capability_probe(&self.id, window)
            .await
    }

    pub async fn execute_policy(
        &self,
        request: &ComputerExecutionRequest,
    ) -> Result<ComputerExecutionResult, ComputerError> {
        self.ensure_open()?;
        self.backend
            .lock()
            .await
            .execute_policy(&self.id, request)
            .await
    }

    pub async fn cleanup_pressed(&self) -> Result<(), ComputerError> {
        self.ensure_open()?;
        self.backend.lock().await.cleanup_pressed(&self.id).await
    }

    pub async fn close(&self) -> Result<(), ComputerError> {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.backend.lock().await.close_session(&self.id).await?;
        }
        Ok(())
    }
}

// Session closure is explicit. `ComputerSession` is cloneable so callers can
// hold a request-local handle across an await; dropping such a clone must not
// invalidate the shared session. The owning runtime/sidecar closes sessions
// through `close`, and runtime shutdown invalidates the backend lifecycle.

#[cfg(windows)]
mod win_native;

#[cfg(windows)]
pub use win_native::{
    NativeCaptureProfile, NativeDisplayInfo, NativeFrameStoreStats, NativePngMode, WinNativeBackend,
};

#[cfg(target_os = "macos")]
mod mac_native;

#[cfg(target_os = "macos")]
pub use mac_native::{MacNativeBackend, NativeDisplayInfo, NativeFrameStoreStats};

#[cfg(target_os = "macos")]
pub type WinNativeBackend = MacNativeBackend;

#[cfg(not(any(windows, target_os = "macos")))]
pub struct WinNativeBackend;

#[cfg(not(any(windows, target_os = "macos")))]
impl WinNativeBackend {
    pub fn new() -> Self {
        Self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alice_computer_use_core::{
        ActionStatus, CaptureFrameMetadata, CoordinateSpace, DpiScale, FrameId, FramePixelFormat,
        Point, Rect, ScreenshotMetadata, Size,
    };

    struct MockBackend {
        initialized: bool,
        shutdowns: usize,
    }

    #[async_trait]
    impl ComputerBackend for MockBackend {
        async fn initialize(&mut self) -> Result<(), ComputerError> {
            self.initialized = true;
            Ok(())
        }

        async fn shutdown(&mut self) -> Result<(), ComputerError> {
            self.initialized = false;
            self.shutdowns += 1;
            Ok(())
        }

        fn capabilities(&self) -> Vec<Capability> {
            Vec::new()
        }

        async fn enumerate_screens(
            &mut self,
            _session: &ComputerSessionId,
        ) -> Result<Vec<Screen>, ComputerError> {
            Ok(vec![Screen {
                id: ScreenId::new("mock"),
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
            }])
        }

        async fn enumerate_windows(
            &mut self,
            _session: &ComputerSessionId,
        ) -> Result<Vec<Window>, ComputerError> {
            Ok(Vec::new())
        }

        async fn screenshot(
            &mut self,
            _session: &ComputerSessionId,
            _screen: &ScreenId,
        ) -> Result<Screenshot, ComputerError> {
            Ok(Screenshot {
                metadata: ScreenshotMetadata {
                    frame_id: FrameId::new("mock-frame"),
                    display_id: Some(ScreenId::new("mock")),
                    width: 1,
                    height: 1,
                    mime_type: "image/png".into(),
                    coordinate_space: CoordinateSpace::ScreenshotPixel,
                    desktop_origin: Point { x: 0.0, y: 0.0 },
                    dpi: DpiScale::ONE,
                    scale: DpiScale::ONE,
                    pixel_to_desktop_scale: Some(DpiScale::ONE),
                    captured_at: None,
                },
                bytes: vec![1],
            })
        }

        async fn capture_frame(
            &mut self,
            _session: &ComputerSessionId,
            _screen: &ScreenId,
        ) -> Result<CaptureFrameMetadata, ComputerError> {
            Ok(CaptureFrameMetadata {
                frame_id: FrameId::new("mock-frame"),
                display_id: ScreenId::new("mock"),
                topology_generation: 1,
                coordinate_space: CoordinateSpace::ScreenshotPixel,
                desktop_origin: Point { x: 0.0, y: 0.0 },
                width: 1,
                height: 1,
                pixel_format: FramePixelFormat::Bgra8,
                stride: 4,
                dpi: DpiScale::ONE,
                scale: DpiScale::ONE,
                pixel_to_desktop_scale: Some(DpiScale::ONE),
                captured_at: None,
                content_revision: 1,
                stale_topology: false,
            })
        }

        async fn execute(
            &mut self,
            _session: &ComputerSessionId,
            _action: &ComputerAction,
        ) -> Result<ComputerActionResult, ComputerError> {
            Ok(ComputerActionResult {
                status: ActionStatus::Performed,
                observation: None,
                backend_detail: None,
            })
        }
    }

    #[tokio::test]
    async fn runtime_reinitializes_after_shutdown_and_sessions_are_explicit() {
        let runtime = ComputerRuntime::new(MockBackend {
            initialized: false,
            shutdowns: 0,
        });
        assert!(matches!(
            runtime.start_session().await,
            Err(ComputerError::NotInitialized)
        ));
        runtime.initialize().await.unwrap();
        let session = runtime.start_session().await.unwrap();
        assert!(!session.id().as_str().is_empty());
        let observation = session.observe().await.unwrap();
        assert_eq!(observation.screens.len(), 1);
        session.close().await.unwrap();
        assert!(matches!(
            session.observe().await,
            Err(ComputerError::SessionClosed)
        ));
        runtime.shutdown().await.unwrap();
        runtime.initialize().await.unwrap();
        runtime.shutdown().await.unwrap();
    }
}
