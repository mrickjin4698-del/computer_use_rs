//! Windows UI Automation provider for bounded R1 observation and R2 actions.
//!
//! UIA/COM objects are confined to this module and one operation. Internal
//! RuntimeIds are used only for exact action resolution; no HWND, UIA
//! interface, RuntimeId, SAFEARRAY, or COM pointer is placed in Core/RPC.

use super::NativeDisplayInfo;
use alice_computer_use_core::{
    ComputerElement, ComputerError, ComputerSessionId, Coordinate, CoordinateSpace, ElementId,
    PixelTarget, Point, SemanticAction, SemanticActionResult, SemanticActionStatus,
    SemanticActionTiming, SemanticActionVerification, SemanticCapabilities, SemanticObservation,
    SemanticObservationLimits, SemanticObservationMetadata, Size, WindowId,
};
use std::{
    collections::{hash_map::DefaultHasher, HashMap, HashSet},
    hash::{Hash, Hasher},
    time::{Duration, Instant, SystemTime},
};
use windows::{
    core::{BSTR, GUID},
    Win32::{
        Foundation::RECT,
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        System::Ole::{
            SafeArrayDestroy, SafeArrayGetDim, SafeArrayGetElement, SafeArrayGetLBound,
            SafeArrayGetUBound,
        },
        UI::Accessibility::{
            ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded,
            ExpandCollapseState_PartiallyExpanded, IUIAutomation, IUIAutomationElement,
            IUIAutomationExpandCollapsePattern, IUIAutomationInvokePattern,
            IUIAutomationRangeValuePattern, IUIAutomationScrollItemPattern,
            IUIAutomationScrollPattern, IUIAutomationSelectionItemPattern,
            IUIAutomationTextPattern, IUIAutomationTogglePattern, IUIAutomationTreeWalker,
            IUIAutomationValuePattern, ToggleState_Off, ToggleState_On, UIA_ButtonControlTypeId,
            UIA_CheckBoxControlTypeId, UIA_ComboBoxControlTypeId, UIA_CustomControlTypeId,
            UIA_DocumentControlTypeId, UIA_EditControlTypeId, UIA_ExpandCollapsePatternId,
            UIA_InvokePatternId, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
            UIA_MenuBarControlTypeId, UIA_MenuControlTypeId, UIA_MenuItemControlTypeId,
            UIA_PaneControlTypeId, UIA_RadioButtonControlTypeId, UIA_RangeValuePatternId,
            UIA_ScrollItemPatternId, UIA_ScrollPatternId, UIA_SelectionItemPatternId,
            UIA_SliderControlTypeId, UIA_TabControlTypeId, UIA_TabItemControlTypeId,
            UIA_TextControlTypeId, UIA_TextPatternId, UIA_TogglePatternId, UIA_TreeControlTypeId,
            UIA_TreeItemControlTypeId, UIA_ValuePatternId, UIA_WindowControlTypeId,
        },
        UI::WindowsAndMessaging::GetForegroundWindow,
    },
};

const CLSID_CUI_AUTOMATION: GUID = GUID::from_u128(0xff48dba4_60ef_4201_aa87_54103eef594e);
const UIA_E_ELEMENT_NOT_AVAILABLE: u32 = 0x8004_0201;
const MAX_TEXT_SUMMARY_UTF16: usize = 4096;
const MAX_PROPERTY_TEXT_UTF16: usize = 1024;
const VALUE_VERIFICATION_RETRY_DELAYS_MS: &[u64] = &[25, 75, 150, 250];

#[derive(Default)]
pub(crate) struct UiaStore {
    sessions: HashMap<ComputerSessionId, SessionState>,
}

struct SessionState {
    nonce: u64,
    generation: u64,
    current: HashMap<ElementId, UiaElementRecord>,
    snapshot: Vec<ComputerElement>,
    observed_window: Option<WindowId>,
}

struct TreeNode {
    element: IUIAutomationElement,
    semantic: ComputerElement,
    password: bool,
}

#[derive(Clone)]
struct UiaElementRecord {
    semantic: ComputerElement,
    password: bool,
    runtime_id: Vec<i32>,
    hwnd: isize,
    window_id: WindowId,
}

impl UiaStore {
    pub(crate) fn current_generation(&self, session_id: &ComputerSessionId) -> Option<u64> {
        self.sessions.get(session_id).map(|state| state.generation)
    }

    pub(crate) fn observe(
        &mut self,
        display: &NativeDisplayInfo,
        hwnd: windows::Win32::Foundation::HWND,
        session_id: &ComputerSessionId,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Result<SemanticObservation, ComputerError> {
        let state = self
            .sessions
            .entry(session_id.clone())
            .or_insert_with(|| SessionState {
                nonce: session_nonce(session_id),
                generation: 0,
                current: HashMap::new(),
                snapshot: Vec::new(),
                observed_window: None,
            });
        state.generation = state.generation.saturating_add(1);
        let generation = state.generation;
        let nonce = state.nonce;
        let limits = limits.bounded();
        let mut current = HashMap::new();
        let result = observe_com(
            display,
            hwnd,
            session_id,
            window_id,
            limits,
            nonce,
            generation,
            &mut current,
        );
        if let Ok(observation) = &result {
            state.current = current;
            state.snapshot = observation.elements.clone();
            state.observed_window = Some(window_id.clone());
        } else {
            state.current.clear();
            state.snapshot.clear();
            state.observed_window = None;
        }
        result
    }

    pub(crate) fn validate(
        &self,
        session_id: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<(), ComputerError> {
        let Some(state) = self.sessions.get(session_id) else {
            return Err(ComputerError::UnknownElement(format!(
                "element is not known for session {session_id}"
            )));
        };
        let prefix = format!("uia-{:016x}-", state.nonce);
        let Some(remainder) = element_id.as_str().strip_prefix(&prefix) else {
            return Err(ComputerError::UnknownElement(
                "element token is not scoped to this session".into(),
            ));
        };
        let Some(generation) = remainder
            .split('-')
            .next()
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Err(ComputerError::UnknownElement(
                "element token has an invalid generation".into(),
            ));
        };
        if generation < state.generation {
            return Err(ComputerError::StaleElement(format!(
                "element belongs to observation generation {generation}; current generation is {}",
                state.generation
            )));
        }
        if generation != state.generation || !state.current.contains_key(element_id) {
            return Err(ComputerError::UnknownElement(
                "element token is not valid in the current observation".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn element_window(
        &self,
        session_id: &ComputerSessionId,
        element_id: &ElementId,
    ) -> Result<WindowId, ComputerError> {
        let state = self.sessions.get(session_id).ok_or_else(|| {
            ComputerError::UnknownElement("element session is no longer valid".into())
        })?;
        let Some(record) = state.current.get(element_id) else {
            return Err(match element_status_for_generation(state, element_id) {
                SemanticActionStatus::StaleElement => ComputerError::StaleElement(
                    "element belongs to an older observation generation".into(),
                ),
                _ => ComputerError::UnknownElement(
                    "element is not valid in the current observation".into(),
                ),
            });
        };
        Ok(record.window_id.clone())
    }

    pub(crate) fn pixel_target(
        &self,
        session_id: &ComputerSessionId,
        element_id: &ElementId,
        display: &NativeDisplayInfo,
    ) -> Result<PixelTarget, ComputerError> {
        let state = self.sessions.get(session_id).ok_or_else(|| {
            ComputerError::UnknownElement("element session is no longer valid".into())
        })?;
        let record = match state.current.get(element_id) {
            Some(record) => record,
            None => {
                return Err(match element_status_for_generation(state, element_id) {
                    SemanticActionStatus::StaleElement => ComputerError::StaleElement(
                        "pixel target belongs to an older observation generation".into(),
                    ),
                    _ => ComputerError::UnknownElement(
                        "element is not valid in the current observation".into(),
                    ),
                })
            }
        };
        let bounds =
            record
                .semantic
                .bounds
                .clone()
                .ok_or_else(|| ComputerError::CapabilityGap {
                    capability: "pixel_target.bounds".into(),
                    detail: "semantic element has no current bounds".into(),
                })?;
        if record.semantic.offscreen
            || bounds.extent.width <= 0.0
            || bounds.extent.height <= 0.0
            || bounds.dpi.x <= 0.0
            || bounds.dpi.y <= 0.0
            || bounds.extent.width.is_nan()
            || bounds.extent.height.is_nan()
        {
            return Err(ComputerError::InvalidCoordinate(
                "semantic pixel target is offscreen or has invalid bounds metadata".into(),
            ));
        }
        let center = Coordinate {
            space: bounds.space,
            point: Point {
                x: bounds.point.x + bounds.extent.width / 2.0,
                y: bounds.point.y + bounds.extent.height / 2.0,
            },
            extent: display.virtual_desktop_bounds.size,
            dpi: alice_computer_use_core::DpiScale::ONE,
            display_id: None,
            frame_id: None,
        };
        Ok(PixelTarget {
            element_id: element_id.clone(),
            window_id: record.window_id.clone(),
            bounds,
            center,
            generation: state.generation,
        })
    }

    pub(crate) fn current_snapshot(
        &self,
        session_id: &ComputerSessionId,
    ) -> Result<(Vec<ComputerElement>, u64), ComputerError> {
        let state = self.sessions.get(session_id).ok_or_else(|| {
            ComputerError::UnknownElement("element session is no longer valid".into())
        })?;
        Ok((state.snapshot.clone(), state.generation))
    }

    /// Read-only capability input from the current authoritative snapshot.
    /// A probe must not create a new semantic generation: doing so would make
    /// an element returned by the immediately preceding observation stale
    /// before the action reaches dispatch.
    pub(crate) fn current_elements_for_window(
        &self,
        session_id: &ComputerSessionId,
        window_id: &WindowId,
        limits: SemanticObservationLimits,
    ) -> Option<(Vec<ComputerElement>, u64)> {
        let state = self.sessions.get(session_id)?;
        if state.observed_window.as_ref() != Some(window_id) {
            return None;
        }
        let limits = limits.bounded();
        let mut elements = state
            .snapshot
            .iter()
            .filter(|element| {
                state
                    .current
                    .get(&element.id)
                    .is_some_and(|record| record.window_id.as_str() == window_id.as_str())
            })
            .cloned()
            .collect::<Vec<_>>();
        elements.truncate(limits.max_elements as usize);
        Some((elements, state.generation))
    }

    pub(crate) fn semantic_state_changed(
        before: &[ComputerElement],
        after: &SemanticObservation,
    ) -> bool {
        semantic_signature(before) != semantic_signature(&after.elements)
    }

    #[allow(non_upper_case_globals)]
    pub(crate) fn action(
        &mut self,
        display: &NativeDisplayInfo,
        session_id: &ComputerSessionId,
        action: &SemanticAction,
    ) -> Result<SemanticActionResult, ComputerError> {
        let element_id = action.element_id().clone();
        let (before_generation, record, before_snapshot) = {
            let Some(state) = self.sessions.get(session_id) else {
                return Ok(action_result(
                    action,
                    SemanticActionStatus::UnknownElement,
                    None,
                    None,
                    SemanticActionVerification {
                        detail: Some("element session is no longer valid".into()),
                        ..Default::default()
                    },
                    SemanticActionTiming::default(),
                ));
            };
            let before_generation = state.generation;
            let Some(record) = state.current.get(&element_id).cloned() else {
                let status = element_status_for_generation(state, &element_id);
                return Ok(action_result(
                    action,
                    status,
                    Some(before_generation),
                    None,
                    SemanticActionVerification {
                        detail: Some("element is not valid in the current observation".into()),
                        ..Default::default()
                    },
                    SemanticActionTiming::default(),
                ));
            };
            (before_generation, record, state.snapshot.clone())
        };

        let total_started = Instant::now();
        if matches!(action, SemanticAction::Focus { .. }) {
            let target_window = windows::Win32::Foundation::HWND(record.hwnd as *mut _);
            let foreground = unsafe { GetForegroundWindow() };
            if foreground != target_window {
                return Ok(action_result(
                    action,
                    SemanticActionStatus::WindowNotForeground,
                    Some(before_generation),
                    None,
                    SemanticActionVerification {
                        detail: Some(format!(
                            "containing window {} is not the current foreground window; UIA SetFocus was not attempted",
                            record.window_id
                        )),
                        focused: Some(false),
                        ..Default::default()
                    },
                    SemanticActionTiming {
                        total_micros: total_started.elapsed().as_micros(),
                        ..Default::default()
                    },
                ));
            }
        }

        let com_result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if com_result.0 < 0 {
            return Err(ComputerError::CapabilityGap {
                capability: "semantic_action.uia".into(),
                detail: format!(
                    "COM apartment initialization failed: hresult=0x{:08x}",
                    com_result.0 as u32
                ),
            });
        }

        let resolve_started = Instant::now();
        let automation: IUIAutomation = unsafe {
            match CoCreateInstance(&CLSID_CUI_AUTOMATION, None, CLSCTX_INPROC_SERVER) {
                Ok(automation) => automation,
                Err(error) => {
                    CoUninitialize();
                    return Err(uia_action_error("CoCreateInstance(CUIAutomation)", error));
                }
            }
        };
        let element = match unsafe {
            resolve_runtime_element(
                &automation,
                windows::Win32::Foundation::HWND(record.hwnd as *mut _),
                &record.runtime_id,
            )
        } {
            Ok(element) => element,
            Err(error) => {
                let resolve_micros = resolve_started.elapsed().as_micros();
                unsafe { CoUninitialize() };
                return Ok(action_result(
                    action,
                    SemanticActionStatus::ElementUnavailable,
                    Some(before_generation),
                    None,
                    SemanticActionVerification {
                        detail: Some(error.to_string()),
                        ..Default::default()
                    },
                    SemanticActionTiming {
                        element_resolve_micros: resolve_micros,
                        total_micros: total_started.elapsed().as_micros(),
                        ..Default::default()
                    },
                ));
            }
        };
        let resolve_micros = resolve_started.elapsed().as_micros();
        let mut pattern_acquire_micros = 0;
        let mut action_micros = 0;
        let mut verification_micros = 0;
        let mut refresh_micros = 0;
        let mut attempted = false;
        let mut result = match action {
            SemanticAction::Focus { .. } => {
                if !record.semantic.focusable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is not keyboard focusable".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.enabled {
                    action_result(
                        action,
                        SemanticActionStatus::Disabled,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is disabled".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    attempted = true;
                    let action_started = Instant::now();
                    let set_focus = unsafe { element.SetFocus() };
                    action_micros = action_started.elapsed().as_micros();
                    let verify_started = Instant::now();
                    let focused = unsafe { element.CurrentHasKeyboardFocus() }
                        .map(|value| value.as_bool())
                        .unwrap_or(false);
                    verification_micros = verify_started.elapsed().as_micros();
                    match set_focus {
                        Ok(()) if focused => action_result(
                            action,
                            SemanticActionStatus::Performed,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                verified: true,
                                focused: Some(true),
                                detail: Some("UIA SetFocus verified by HasKeyboardFocus".into()),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                        Ok(()) => action_result(
                            action,
                            SemanticActionStatus::VerificationFailed,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                focused: Some(false),
                                detail: Some(
                                    "UIA SetFocus returned but focus was not observed".into(),
                                ),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::FocusDenied,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("UIA SetFocus failed: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::Invoke { .. } => {
                if !record.semantic.capabilities.invokable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no InvokePattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.enabled {
                    action_result(
                        action,
                        SemanticActionStatus::Disabled,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is disabled".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element
                            .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            attempted = true;
                            let action_started = Instant::now();
                            let invoked = unsafe { pattern.Invoke() };
                            action_micros = action_started.elapsed().as_micros();
                            match invoked {
                                Ok(()) => {
                                    let refresh_started = Instant::now();
                                    let mut refreshed_elements = HashMap::new();
                                    let refresh_result = observe_com(
                                        display,
                                        windows::Win32::Foundation::HWND(record.hwnd as *mut _),
                                        session_id,
                                        &record.window_id,
                                        SemanticObservationLimits::default(),
                                        session_nonce(session_id),
                                        before_generation.saturating_add(1),
                                        &mut refreshed_elements,
                                    );
                                    refresh_micros = refresh_started.elapsed().as_micros();
                                    match refresh_result {
                                        Ok(observation) => {
                                            let changed = semantic_signature(&before_snapshot)
                                                != semantic_signature(&observation.elements);
                                            self.replace_snapshot(
                                                session_id,
                                                refreshed_elements,
                                                observation.elements,
                                                before_generation.saturating_add(1),
                                            );
                                            action_result(
                                                action,
                                                if changed {
                                                    SemanticActionStatus::Performed
                                                } else {
                                                    SemanticActionStatus::VerificationFailed
                                                },
                                                Some(before_generation),
                                                Some(before_generation.saturating_add(1)),
                                                SemanticActionVerification {
                                                    verified: changed,
                                                    state_changed: Some(changed),
                                                    detail: Some(if changed {
                                                        "Invoke followed by changed semantic observation".into()
                                                    } else {
                                                        "Invoke completed but no semantic state change was observed".into()
                                                    }),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            )
                                        }
                                        Err(error) => {
                                            self.invalidate(session_id);
                                            action_result(
                                                action,
                                                SemanticActionStatus::OutcomeUnknown,
                                                Some(before_generation),
                                                self.generation(session_id),
                                                SemanticActionVerification {
                                                    detail: Some(format!(
                                                        "Invoke was sent but post-action observation failed: {error}"
                                                    )),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            )
                                        }
                                    }
                                }
                                Err(error) => action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(format!("UIA Invoke failed: {error}")),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                ),
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("InvokePattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::SetValue { value, .. } => {
                if record.password {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("SetValue is disabled for password elements".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.capabilities.editable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is not editable".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            let read_only =
                                unsafe { pattern.CurrentIsReadOnly().map(|value| value.as_bool()) }
                                    .unwrap_or(false);
                            if read_only {
                                action_result(
                                    action,
                                    SemanticActionStatus::ReadOnly,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some("ValuePattern reports IsReadOnly".into()),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                )
                            } else {
                                attempted = true;
                                let bstr = BSTR::from(value.as_str());
                                let action_started = Instant::now();
                                let set_value = unsafe { pattern.SetValue(&bstr) };
                                action_micros = action_started.elapsed().as_micros();
                                match set_value {
                                    Ok(()) => {
                                        let verify_started = Instant::now();
                                        let mut last_generation = None;
                                        let mut last_observed_value = None;
                                        let mut last_error = None;
                                        let mut verified_after = None;
                                        for attempt in 0..=VALUE_VERIFICATION_RETRY_DELAYS_MS.len()
                                        {
                                            if attempt > 0 {
                                                std::thread::sleep(Duration::from_millis(
                                                    VALUE_VERIFICATION_RETRY_DELAYS_MS[attempt - 1],
                                                ));
                                            }
                                            match self.refresh_after_action_target(
                                                display,
                                                session_id,
                                                &record,
                                                before_generation,
                                                &automation,
                                            ) {
                                                Ok((
                                                    generation,
                                                    _fresh_element,
                                                    fresh_uia_element,
                                                )) => {
                                                    last_generation = Some(generation);
                                                    match unsafe {
                                                        fresh_uia_element
                                                            .GetCurrentPatternAs::<
                                                                IUIAutomationValuePattern,
                                                            >(UIA_ValuePatternId)
                                                            .and_then(|pattern| {
                                                                pattern.CurrentValue()
                                                            })
                                                    } {
                                                        Ok(actual) => {
                                                            let observed = String::from_utf16_lossy(
                                                                actual.as_wide(),
                                                            );
                                                            let matches = text_value_matches(
                                                                &observed, value,
                                                            );
                                                            last_observed_value = Some(observed);
                                                            if matches {
                                                                verified_after = Some(attempt + 1);
                                                                break;
                                                            }
                                                        }
                                                        Err(error) => {
                                                            last_error = Some(format!(
                                                                "fresh ValuePattern readback failed: {error}"
                                                            ));
                                                        }
                                                    }
                                                }
                                                Err(error) => {
                                                    last_error = Some(format!(
                                                        "fresh-element verification failed: {error}"
                                                    ));
                                                }
                                            }
                                        }
                                        verification_micros = verify_started.elapsed().as_micros();
                                        if let Some(observations) = verified_after {
                                            action_result(
                                                action,
                                                SemanticActionStatus::Performed,
                                                Some(before_generation),
                                                last_generation,
                                                SemanticActionVerification {
                                                    verified: true,
                                                    observed_value: last_observed_value,
                                                    detail: Some(format!(
                                                        "ValuePattern.SetValue was dispatched once and matched after {observations} bounded readback observation(s)"
                                                    )),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            )
                                        } else if last_observed_value.is_some() {
                                            action_result(
                                                action,
                                                SemanticActionStatus::VerificationFailed,
                                                Some(before_generation),
                                                last_generation,
                                                SemanticActionVerification {
                                                    verified: false,
                                                    observed_value: last_observed_value,
                                                    detail: Some(format!(
                                                        "ValuePattern.SetValue was dispatched once but bounded readback did not match; do not replay without a fresh observation{}",
                                                        last_error
                                                            .map(|error| format!("; last_error={error}"))
                                                            .unwrap_or_default()
                                                    )),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            )
                                        } else {
                                            self.invalidate(session_id);
                                            action_result(
                                                action,
                                                SemanticActionStatus::OutcomeUnknown,
                                                Some(before_generation),
                                                self.generation(session_id),
                                                SemanticActionVerification {
                                                    detail: Some(format!(
                                                        "SetValue was dispatched once but no readback was available after bounded verification: {}",
                                                        last_error.unwrap_or_else(|| "unknown verification failure".into())
                                                    )),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            )
                                        }
                                    }
                                    Err(error) => action_result(
                                        action,
                                        SemanticActionStatus::OutcomeUnknown,
                                        Some(before_generation),
                                        None,
                                        SemanticActionVerification {
                                            detail: Some(format!("UIA SetValue failed: {error}")),
                                            ..Default::default()
                                        },
                                        SemanticActionTiming::default(),
                                    ),
                                }
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::Unsupported,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("ValuePattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::Toggle { .. } => {
                if !record.semantic.capabilities.toggleable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no TogglePattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.enabled {
                    action_result(
                        action,
                        SemanticActionStatus::Disabled,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is disabled".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element
                            .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            attempted = true;
                            let action_started = Instant::now();
                            let toggled = unsafe { pattern.Toggle() };
                            action_micros = action_started.elapsed().as_micros();
                            match toggled {
                                Ok(()) => match unsafe { pattern.CurrentToggleState() } {
                                    Ok(state) => {
                                        let same_element_state = match state {
                                            ToggleState_On => Some(true),
                                            ToggleState_Off => Some(false),
                                            _ => None,
                                        };
                                        match self.refresh_after_action_target(
                                            display,
                                            session_id,
                                            &record,
                                            before_generation,
                                            &automation,
                                        ) {
                                            Ok((generation, fresh_element, fresh_uia_element)) => {
                                                let fresh_state = unsafe {
                                                    fresh_uia_element
                                                        .GetCurrentPatternAs::<
                                                            IUIAutomationTogglePattern,
                                                        >(UIA_TogglePatternId)
                                                        .ok()
                                                        .and_then(|pattern| {
                                                            pattern.CurrentToggleState().ok()
                                                        })
                                                        .and_then(|state| match state {
                                                            ToggleState_On => Some(true),
                                                            ToggleState_Off => Some(false),
                                                            _ => None,
                                                        })
                                                };
                                                let _ = fresh_element;
                                                // Some Win32 providers keep the state on the
                                                // already-acquired pattern stale after Toggle().
                                                // The fresh element is the authoritative UIA
                                                // readback; retain the same-element value only as
                                                // forensic evidence and never use BM_SETCHECK or a
                                                // second Toggle to reconcile it.
                                                let fresh_changed = state_changed(
                                                    record.semantic.toggle_state,
                                                    fresh_state,
                                                );
                                                let same_element_changed = state_changed(
                                                    record.semantic.toggle_state,
                                                    same_element_state,
                                                );
                                                // Prefer a fresh target readback. A transient menu
                                                // item may disappear as a direct consequence of
                                                // Toggle(), in which case a changed state read from
                                                // the already-acquired pattern is still positive
                                                // evidence. An unchanged or missing value never
                                                // proves success.
                                                let verified = toggle_transition_proven(
                                                    record.semantic.toggle_state,
                                                    same_element_state,
                                                    fresh_state,
                                                );
                                                action_result(
                                                    action,
                                                    if verified {
                                                        SemanticActionStatus::Performed
                                                    } else {
                                                        SemanticActionStatus::VerificationFailed
                                                    },
                                                    Some(before_generation),
                                                    Some(generation),
                                                    SemanticActionVerification {
                                                        verified,
                                                        state_changed: Some(
                                                            fresh_changed
                                                                || (fresh_state.is_none()
                                                                    && same_element_changed),
                                                        ),
                                                        toggled: fresh_state,
                                                        detail: Some(if verified {
                                                            format!(
                                                                "TogglePattern.Toggle invoked once; same-element={same_element_state:?}, fresh-element={fresh_state:?}, state changed"
                                                            )
                                                        } else {
                                                            format!(
                                                                "TogglePattern state transition was not proven; same-element={same_element_state:?}, fresh-element={fresh_state:?}, before={:?}",
                                                                record.semantic.toggle_state
                                                            )
                                                        }),
                                                        ..Default::default()
                                                    },
                                                    SemanticActionTiming::default(),
                                                )
                                            }
                                            Err(error) => {
                                                let same_element_changed = state_changed(
                                                    record.semantic.toggle_state,
                                                    same_element_state,
                                                );
                                                self.invalidate(session_id);
                                                action_result(
                                                    action,
                                                    if same_element_changed {
                                                        SemanticActionStatus::Performed
                                                    } else {
                                                        SemanticActionStatus::OutcomeUnknown
                                                    },
                                                    Some(before_generation),
                                                    self.generation(session_id),
                                                    SemanticActionVerification {
                                                        verified: same_element_changed,
                                                        state_changed: Some(same_element_changed),
                                                        toggled: same_element_state,
                                                        detail: Some(if same_element_changed {
                                                            format!(
                                                                "TogglePattern invoked once and the acquired pattern reported a state transition before the transient target disappeared; fresh-element refresh failed: {error}"
                                                            )
                                                        } else {
                                                            format!(
                                                                "TogglePattern was sent but fresh-element verification failed: {error}; same-element={same_element_state:?}"
                                                            )
                                                        }),
                                                        ..Default::default()
                                                    },
                                                    SemanticActionTiming::default(),
                                                )
                                            }
                                        }
                                    }
                                    Err(error) => action_result(
                                        action,
                                        SemanticActionStatus::OutcomeUnknown,
                                        Some(before_generation),
                                        None,
                                        SemanticActionVerification {
                                            detail: Some(format!(
                                                "TogglePattern was sent but state readback failed: {error}"
                                            )),
                                            ..Default::default()
                                        },
                                        SemanticActionTiming::default(),
                                    ),
                                },
                                Err(error) => action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(format!("UIA Toggle failed: {error}")),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                ),
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("TogglePattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::Select { .. } => {
                if !record.semantic.capabilities.selectable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no SelectionItemPattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.enabled {
                    action_result(
                        action,
                        SemanticActionStatus::Disabled,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is disabled".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(
                            UIA_SelectionItemPatternId,
                        )
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            attempted = true;
                            let action_started = Instant::now();
                            let selected = unsafe { pattern.Select() };
                            action_micros = action_started.elapsed().as_micros();
                            match selected {
                                Ok(()) => match self.refresh_after_action_target(
                                    display,
                                    session_id,
                                    &record,
                                    before_generation,
                                    &automation,
                                ) {
                                    Ok((generation, fresh_element, fresh_uia_element)) => {
                                        let fresh_selected = unsafe {
                                            fresh_uia_element
                                                .GetCurrentPatternAs::<
                                                    IUIAutomationSelectionItemPattern,
                                                >(UIA_SelectionItemPatternId)
                                                .ok()
                                                .and_then(|pattern| {
                                                    pattern.CurrentIsSelected().ok()
                                                })
                                                .map(|value| value.as_bool())
                                        };
                                        let verified = fresh_selected == Some(true);
                                        action_result(
                                            action,
                                            if verified {
                                                SemanticActionStatus::Performed
                                            } else {
                                                SemanticActionStatus::VerificationFailed
                                            },
                                            Some(before_generation),
                                            Some(generation),
                                            SemanticActionVerification {
                                                verified,
                                                selected: fresh_selected,
                                                state_changed: Some(
                                                    record.semantic.selected != fresh_selected,
                                                ),
                                                detail: Some(format!(
                                                    "SelectionItemPattern.Select invoked once; fresh element selected={fresh_selected:?}; observed element selected={:?}",
                                                    fresh_element.selected
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                    Err(error) => {
                                        self.invalidate(session_id);
                                        action_result(
                                            action,
                                            SemanticActionStatus::OutcomeUnknown,
                                            Some(before_generation),
                                            self.generation(session_id),
                                            SemanticActionVerification {
                                                detail: Some(format!(
                                                    "SelectionItemPattern was sent but fresh-element verification failed: {error}"
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                },
                                Err(error) => action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(format!("UIA Select failed: {error}")),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                ),
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("SelectionItemPattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::Expand { .. } | SemanticAction::Collapse { .. } => {
                let expand = matches!(action, SemanticAction::Expand { .. });
                if !record.semantic.capabilities.expandable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no ExpandCollapsePattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !record.semantic.enabled {
                    action_result(
                        action,
                        SemanticActionStatus::Disabled,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element is disabled".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                            UIA_ExpandCollapsePatternId,
                        )
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            attempted = true;
                            let action_started = Instant::now();
                            let changed = unsafe {
                                if expand {
                                    pattern.Expand()
                                } else {
                                    pattern.Collapse()
                                }
                            };
                            action_micros = action_started.elapsed().as_micros();
                            match changed {
                                Ok(()) => match self.refresh_after_action_target(
                                    display,
                                    session_id,
                                    &record,
                                    before_generation,
                                    &automation,
                                ) {
                                    Ok((generation, fresh_element, fresh_uia_element)) => {
                                        let fresh_expanded = unsafe {
                                            fresh_uia_element
                                                .GetCurrentPatternAs::<
                                                    IUIAutomationExpandCollapsePattern,
                                                >(UIA_ExpandCollapsePatternId)
                                                .ok()
                                                .and_then(|pattern| {
                                                    pattern.CurrentExpandCollapseState().ok()
                                                })
                                                .and_then(|state| match state {
                                                    ExpandCollapseState_Expanded
                                                    | ExpandCollapseState_PartiallyExpanded => {
                                                        Some(true)
                                                    }
                                                    ExpandCollapseState_Collapsed => Some(false),
                                                    _ => None,
                                                })
                                        };
                                        let verified = fresh_expanded == Some(expand);
                                        action_result(
                                            action,
                                            if verified {
                                                SemanticActionStatus::Performed
                                            } else {
                                                SemanticActionStatus::VerificationFailed
                                            },
                                            Some(before_generation),
                                            Some(generation),
                                            SemanticActionVerification {
                                                verified,
                                                expanded: fresh_expanded,
                                                state_changed: Some(
                                                    fresh_expanded != record.semantic.expanded,
                                                ),
                                                detail: Some(format!(
                                                    "{} invoked once; fresh element expanded={fresh_expanded:?}; observed element expanded={:?}",
                                                    if expand { "Expand" } else { "Collapse" },
                                                    fresh_element.expanded
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                    Err(error) => {
                                        self.invalidate(session_id);
                                        action_result(
                                            action,
                                            SemanticActionStatus::OutcomeUnknown,
                                            Some(before_generation),
                                            self.generation(session_id),
                                            SemanticActionVerification {
                                                detail: Some(format!(
                                                    "ExpandCollapsePattern was sent but fresh-element verification failed: {error}"
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                },
                                Err(error) => action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(format!(
                                            "UIA {} failed: {error}",
                                            if expand { "Expand" } else { "Collapse" }
                                        )),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                ),
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("ExpandCollapsePattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::SetRangeValue { value, .. } => {
                if !record.semantic.capabilities.range_adjustable {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no RangeValuePattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else if !value.is_finite() {
                    action_result(
                        action,
                        SemanticActionStatus::InvalidValue,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            range_value: Some(*value),
                            detail: Some("range value must be finite".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(
                            UIA_RangeValuePatternId,
                        )
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            let minimum = unsafe { pattern.CurrentMinimum() };
                            let maximum = unsafe { pattern.CurrentMaximum() };
                            if let (Ok(minimum), Ok(maximum)) = (minimum, maximum) {
                                if *value < minimum || *value > maximum {
                                    action_result(
                                    action,
                                    SemanticActionStatus::InvalidValue,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        range_value: Some(*value),
                                        detail: Some(format!(
                                            "requested range value {value} is outside [{minimum}, {maximum}]; SetValue was not called"
                                        )),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                )
                                } else {
                                    let read_only = unsafe {
                                        pattern.CurrentIsReadOnly().map(|value| value.as_bool())
                                    }
                                    .unwrap_or(false);
                                    if read_only {
                                        action_result(
                                            action,
                                            SemanticActionStatus::ReadOnly,
                                            Some(before_generation),
                                            None,
                                            SemanticActionVerification {
                                                detail: Some(
                                                    "RangeValuePattern reports IsReadOnly".into(),
                                                ),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    } else {
                                        attempted = true;
                                        let action_started = Instant::now();
                                        let set_value = unsafe { pattern.SetValue(*value) };
                                        action_micros = action_started.elapsed().as_micros();
                                        match set_value {
                                            Ok(()) => match self.refresh_after_action_target(
                                                display,
                                                session_id,
                                                &record,
                                                before_generation,
                                                &automation,
                                            ) {
                                                Ok((
                                                    generation,
                                                    fresh_element,
                                                    fresh_uia_element,
                                                )) => {
                                                    let fresh_value = unsafe {
                                                        fresh_uia_element
                                                        .GetCurrentPatternAs::<
                                                            IUIAutomationRangeValuePattern,
                                                        >(UIA_RangeValuePatternId)
                                                        .ok()
                                                        .and_then(|pattern| pattern.CurrentValue().ok())
                                                    };
                                                    let verified = fresh_value
                                                        .map(|actual| {
                                                            (actual - *value).abs() <= 0.000_001
                                                        })
                                                        .unwrap_or(false);
                                                    let state_changed = fresh_value.map(|actual| {
                                                        record
                                                            .semantic
                                                            .range_value
                                                            .map(|before| {
                                                                (before - actual).abs() > 0.000_001
                                                            })
                                                            .unwrap_or(true)
                                                    });
                                                    action_result(
                                                    action,
                                                    if verified {
                                                        SemanticActionStatus::Performed
                                                    } else {
                                                        SemanticActionStatus::VerificationFailed
                                                    },
                                                    Some(before_generation),
                                                    Some(generation),
                                                    SemanticActionVerification {
                                                        verified,
                                                        range_value: fresh_value,
                                                        state_changed,
                                                        detail: Some(format!(
                                                            "RangeValuePattern.SetValue invoked once; fresh value={fresh_value:?}; observed value={:?}; requested={value}",
                                                            fresh_element.range_value
                                                        )),
                                                        ..Default::default()
                                                    },
                                                    SemanticActionTiming::default(),
                                                )
                                                }
                                                Err(error) => {
                                                    self.invalidate(session_id);
                                                    action_result(
                                                    action,
                                                    SemanticActionStatus::OutcomeUnknown,
                                                    Some(before_generation),
                                                    self.generation(session_id),
                                                    SemanticActionVerification {
                                                        detail: Some(format!(
                                                            "RangeValuePattern was sent but fresh-element verification failed: {error}"
                                                        )),
                                                        ..Default::default()
                                                    },
                                                    SemanticActionTiming::default(),
                                                )
                                                }
                                            },
                                            Err(error) => action_result(
                                                action,
                                                SemanticActionStatus::OutcomeUnknown,
                                                Some(before_generation),
                                                None,
                                                SemanticActionVerification {
                                                    detail: Some(format!(
                                                        "UIA RangeValue SetValue failed: {error}"
                                                    )),
                                                    ..Default::default()
                                                },
                                                SemanticActionTiming::default(),
                                            ),
                                        }
                                    }
                                }
                            } else {
                                action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(
                                            "RangeValuePattern did not expose min/max".into(),
                                        ),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                )
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("RangeValuePattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
            SemanticAction::ScrollIntoView { .. } => {
                if !record.semantic.capabilities.scroll_into_view {
                    action_result(
                        action,
                        SemanticActionStatus::Unsupported,
                        Some(before_generation),
                        None,
                        SemanticActionVerification {
                            detail: Some("element has no ScrollItemPattern".into()),
                            ..Default::default()
                        },
                        SemanticActionTiming::default(),
                    )
                } else {
                    let pattern_started = Instant::now();
                    let pattern = unsafe {
                        element.GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(
                            UIA_ScrollItemPatternId,
                        )
                    };
                    pattern_acquire_micros = pattern_started.elapsed().as_micros();
                    match pattern {
                        Ok(pattern) => {
                            attempted = true;
                            let action_started = Instant::now();
                            let scrolled = unsafe { pattern.ScrollIntoView() };
                            action_micros = action_started.elapsed().as_micros();
                            match scrolled {
                                Ok(()) => match self.refresh_after_action_target(
                                    display,
                                    session_id,
                                    &record,
                                    before_generation,
                                    &automation,
                                ) {
                                    Ok((generation, fresh_element, fresh_uia_element)) => {
                                        let fresh_offscreen = unsafe {
                                            fresh_uia_element
                                                .CurrentIsOffscreen()
                                                .ok()
                                                .map(|value| value.as_bool())
                                        };
                                        let verified = fresh_offscreen == Some(false);
                                        action_result(
                                            action,
                                            if verified {
                                                SemanticActionStatus::Performed
                                            } else {
                                                SemanticActionStatus::VerificationFailed
                                            },
                                            Some(before_generation),
                                            Some(generation),
                                            SemanticActionVerification {
                                                verified,
                                                offscreen: fresh_offscreen,
                                                detail: Some(format!(
                                                    "ScrollItemPattern.ScrollIntoView invoked once; fresh IsOffscreen={fresh_offscreen:?}; observed IsOffscreen={}",
                                                    fresh_element.offscreen
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                    Err(error) => {
                                        self.invalidate(session_id);
                                        action_result(
                                            action,
                                            SemanticActionStatus::OutcomeUnknown,
                                            Some(before_generation),
                                            self.generation(session_id),
                                            SemanticActionVerification {
                                                detail: Some(format!(
                                                    "ScrollIntoView was sent but fresh-element verification failed: {error}"
                                                )),
                                                ..Default::default()
                                            },
                                            SemanticActionTiming::default(),
                                        )
                                    }
                                },
                                Err(error) => action_result(
                                    action,
                                    SemanticActionStatus::OutcomeUnknown,
                                    Some(before_generation),
                                    None,
                                    SemanticActionVerification {
                                        detail: Some(format!("UIA ScrollIntoView failed: {error}")),
                                        ..Default::default()
                                    },
                                    SemanticActionTiming::default(),
                                ),
                            }
                        }
                        Err(error) => action_result(
                            action,
                            SemanticActionStatus::ElementUnavailable,
                            Some(before_generation),
                            None,
                            SemanticActionVerification {
                                detail: Some(format!("ScrollItemPattern unavailable: {error}")),
                                ..Default::default()
                            },
                            SemanticActionTiming::default(),
                        ),
                    }
                }
            }
        };

        unsafe { CoUninitialize() };
        let preserves_generation = semantic_action_preserves_generation(
            action,
            result.status,
            result.verification.verified,
        );
        let should_invalidate = !preserves_generation
            && (attempted
                || matches!(
                    result.status,
                    SemanticActionStatus::Performed
                        | SemanticActionStatus::VerificationFailed
                        | SemanticActionStatus::OutcomeUnknown
                ));
        if should_invalidate && result.observation_generation_after.is_none() {
            let after = self.invalidate(session_id);
            result.observation_generation_after = after;
        } else if preserves_generation {
            self.mark_focused(session_id, &element_id);
            result.observation_generation_after = Some(before_generation);
        }
        result.timing = SemanticActionTiming {
            element_resolve_micros: resolve_micros,
            pattern_acquire_micros,
            action_micros,
            verification_micros,
            refresh_micros,
            total_micros: total_started.elapsed().as_micros(),
        };
        Ok(result)
    }

    fn refresh_after_action_target(
        &mut self,
        display: &NativeDisplayInfo,
        session_id: &ComputerSessionId,
        record: &UiaElementRecord,
        before_generation: u64,
        automation: &IUIAutomation,
    ) -> Result<(u64, ComputerElement, IUIAutomationElement), ComputerError> {
        let mut refreshed_elements = HashMap::new();
        // A post-action refresh only needs to resolve and verify the target.
        // Keeping the traversal bounded is important for controls such as a
        // Win32 ComboBox: after Expand, its transient popup can expose a
        // provider subtree that is much slower to enumerate than the stable
        // window tree.  The target is a direct fixture child (and production
        // targets are still subject to the bounded observation contract), so
        // this remains well above the target path budget without turning
        // verification into an unbounded synchronous tree walk.
        let observation = observe_com(
            display,
            windows::Win32::Foundation::HWND(record.hwnd as *mut _),
            session_id,
            &record.window_id,
            SemanticObservationLimits {
                max_depth: SemanticObservationLimits::default().max_depth,
                max_elements: 128,
            },
            session_nonce(session_id),
            before_generation.saturating_add(1),
            &mut refreshed_elements,
        )?;
        let generation = observation.metadata.generation;
        let matches = observation
            .elements
            .iter()
            .filter(|element| same_element_identity(&record.semantic, element))
            .cloned()
            .collect::<Vec<_>>();
        let fresh_element = match matches.as_slice() {
            [element] => element.clone(),
            [] => {
                return Err(ComputerError::UnknownElement(
                    "fresh UIA observation did not contain the semantic target identity".into(),
                ))
            }
            _ => {
                // Some native providers expose the same ComboBox more than
                // once while its popup is open.  Semantic properties alone
                // are therefore insufficient to identify the post-action
                // target.  Prefer the exact pre-action RuntimeId; if it is
                // not uniquely present, fail closed instead of verifying a
                // sibling/proxy that merely looks identical.
                let runtime_matches = matches
                    .iter()
                    .filter(|element| {
                        refreshed_elements
                            .get(&element.id)
                            .map(|candidate| candidate.runtime_id == record.runtime_id)
                            .unwrap_or(false)
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                if let Some(element) = runtime_matches.first() {
                    // A Win32 ComboBox may expose several provider aliases
                    // with the same RuntimeId while its popup is open. They
                    // are the same UIA identity for action purposes; the
                    // first control-view occurrence is also the one used by
                    // resolve_runtime_element below.
                    element.clone()
                } else {
                    let candidates = matches
                    .iter()
                    .map(|element| {
                        let runtime_id = refreshed_elements
                            .get(&element.id)
                            .map(|candidate| format!("{:?}", candidate.runtime_id))
                            .unwrap_or_else(|| "<missing>".into());
                        format!(
                            "id={} runtime_id={} parent={:?} bounds={:?} control_type={} name={:?} class={:?} automation_id={:?}",
                            element.id,
                            runtime_id,
                            element.parent_id,
                            element.bounds,
                            element.control_type,
                            element.name,
                            element.class_name,
                            element.automation_id,
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("; ");
                    return Err(ComputerError::UnknownElement(
                    format!(
                        "fresh UIA observation contained multiple elements matching the semantic target identity and no unique RuntimeId match: {candidates}"
                    ),
                ));
                }
            }
        };
        let fresh_record = refreshed_elements
            .values()
            .find(|record| record.semantic.id == fresh_element.id)
            .ok_or_else(|| {
                ComputerError::UnknownElement(
                    "fresh UIA observation lost the target runtime identity".into(),
                )
            })?;
        let fresh_uia_element = unsafe {
            resolve_runtime_element(
                automation,
                windows::Win32::Foundation::HWND(record.hwnd as *mut _),
                &fresh_record.runtime_id,
            )?
        };
        self.replace_snapshot(
            session_id,
            refreshed_elements,
            observation.elements,
            generation,
        );
        Ok((generation, fresh_element, fresh_uia_element))
    }

    pub(crate) fn close_session(&mut self, session_id: &ComputerSessionId) {
        self.sessions.remove(session_id);
    }

    pub(crate) fn clear(&mut self) {
        self.sessions.clear();
    }

    fn generation(&self, session_id: &ComputerSessionId) -> Option<u64> {
        self.sessions.get(session_id).map(|state| state.generation)
    }

    fn invalidate(&mut self, session_id: &ComputerSessionId) -> Option<u64> {
        let state = self.sessions.get_mut(session_id)?;
        state.generation = state.generation.saturating_add(1);
        state.current.clear();
        state.snapshot.clear();
        state.observed_window = None;
        Some(state.generation)
    }

    fn mark_focused(&mut self, session_id: &ComputerSessionId, element_id: &ElementId) {
        let Some(state) = self.sessions.get_mut(session_id) else {
            return;
        };
        for record in state.current.values_mut() {
            record.semantic.focused = record.semantic.id == *element_id;
        }
        for element in &mut state.snapshot {
            element.focused = element.id == *element_id;
        }
    }

    fn replace_snapshot(
        &mut self,
        session_id: &ComputerSessionId,
        current: HashMap<ElementId, UiaElementRecord>,
        snapshot: Vec<ComputerElement>,
        generation: u64,
    ) {
        if let Some(state) = self.sessions.get_mut(session_id) {
            state.current = current;
            state.snapshot = snapshot;
            state.generation = generation;
            state.observed_window = state
                .current
                .values()
                .next()
                .map(|record| record.window_id.clone());
        }
    }
}

fn session_nonce(session_id: &ComputerSessionId) -> u64 {
    let mut hasher = DefaultHasher::new();
    session_id.hash(&mut hasher);
    hasher.finish()
}

fn parse_generation(element_id: &ElementId) -> Option<u64> {
    element_id
        .as_str()
        .split('-')
        .nth(2)
        .and_then(|value| value.parse::<u64>().ok())
}

fn element_status_for_generation(
    state: &SessionState,
    element_id: &ElementId,
) -> SemanticActionStatus {
    let prefix = format!("uia-{:016x}-", state.nonce);
    match element_id
        .as_str()
        .strip_prefix(&prefix)
        .and_then(|_| parse_generation(element_id))
    {
        Some(generation) if generation < state.generation => SemanticActionStatus::StaleElement,
        _ => SemanticActionStatus::UnknownElement,
    }
}

fn action_result(
    action: &SemanticAction,
    status: SemanticActionStatus,
    observation_generation_before: Option<u64>,
    observation_generation_after: Option<u64>,
    verification: SemanticActionVerification,
    timing: SemanticActionTiming,
) -> SemanticActionResult {
    SemanticActionResult {
        action: action.clone(),
        element_id: action.element_id().clone(),
        status,
        verification,
        timing,
        observation_generation_before,
        observation_generation_after,
        security: None,
    }
}

fn text_value_matches(actual: &str, expected: &str) -> bool {
    actual == expected || normalize_newlines(actual) == normalize_newlines(expected)
}

fn state_changed(before: Option<bool>, after: Option<bool>) -> bool {
    before
        .zip(after)
        .is_some_and(|(before, after)| before != after)
}

fn toggle_transition_proven(
    before: Option<bool>,
    same_element_after: Option<bool>,
    fresh_element_after: Option<bool>,
) -> bool {
    match fresh_element_after {
        Some(after) => state_changed(before, Some(after)),
        None => state_changed(before, same_element_after),
    }
}

fn semantic_action_preserves_generation(
    action: &SemanticAction,
    status: SemanticActionStatus,
    verified: bool,
) -> bool {
    matches!(action, SemanticAction::Focus { .. })
        && status == SemanticActionStatus::Performed
        && verified
}

fn normalize_newlines(value: &str) -> String {
    value.replace("\r\n", "\n").replace('\r', "\n")
}

fn same_element_identity(before: &ComputerElement, after: &ComputerElement) -> bool {
    if before.control_type != after.control_type
        || before.name != after.name
        || before.class_name != after.class_name
    {
        return false;
    }
    match before.automation_id.as_deref() {
        Some(automation_id) if !automation_id.is_empty() => {
            after.automation_id.as_deref() == Some(automation_id)
        }
        _ => before.automation_id == after.automation_id,
    }
}

fn semantic_signature(elements: &[ComputerElement]) -> Vec<String> {
    let indexes = elements
        .iter()
        .enumerate()
        .map(|(index, element)| (element.id.clone(), index))
        .collect::<HashMap<_, _>>();
    elements
        .iter()
        .map(|element| {
            let parent = element
                .parent_id
                .as_ref()
                .and_then(|id| indexes.get(id).copied());
            let children = element
                .child_ids
                .iter()
                .filter_map(|id| indexes.get(id).copied())
                .collect::<Vec<_>>();
            format!(
                "{parent:?}|{children:?}|role={:?}|control={:?}|name={:?}|automation={:?}|class={:?}|value={:?}|text={:?}|bounds={:?}|enabled={}|focused={}|focusable={}|offscreen={}|toggle={:?}|selected={:?}|expanded={:?}|range={:?}|min={:?}|max={:?}|caps={:?}",
                element.role,
                element.control_type,
                element.name,
                element.automation_id,
                element.class_name,
                element.value_summary,
                element.text_summary,
                element.bounds,
                element.enabled,
                element.focused,
                element.focusable,
                element.offscreen,
                element.toggle_state,
                element.selected,
                element.expanded,
                element.range_value,
                element.range_minimum,
                element.range_maximum,
                element.capabilities,
            )
        })
        .collect()
}

unsafe fn runtime_id(element: &IUIAutomationElement) -> Result<Vec<i32>, ComputerError> {
    let array = element
        .GetRuntimeId()
        .map_err(|error| uia_error("GetRuntimeId", error))?;
    if array.is_null() {
        return Ok(Vec::new());
    }
    let result = (|| {
        if SafeArrayGetDim(array) != 1 {
            return Err(ComputerError::CapabilityGap {
                capability: "semantic_action.uia_identity".into(),
                detail: "UIA RuntimeId was not one-dimensional".into(),
            });
        }
        let lower = SafeArrayGetLBound(array, 1)
            .map_err(|error| uia_error("SafeArrayGetLBound(RuntimeId)", error))?;
        let upper = SafeArrayGetUBound(array, 1)
            .map_err(|error| uia_error("SafeArrayGetUBound(RuntimeId)", error))?;
        let mut values = Vec::new();
        for index in lower..=upper {
            let mut value = 0i32;
            SafeArrayGetElement(
                array,
                &index,
                (&mut value as *mut i32).cast::<std::ffi::c_void>(),
            )
            .map_err(|error| uia_error("SafeArrayGetElement(RuntimeId)", error))?;
            values.push(value);
        }
        Ok(values)
    })();
    let _ = SafeArrayDestroy(array);
    result
}

unsafe fn resolve_runtime_element(
    automation: &IUIAutomation,
    hwnd: windows::Win32::Foundation::HWND,
    target_runtime_id: &[i32],
) -> Result<IUIAutomationElement, ComputerError> {
    if target_runtime_id.is_empty() {
        return Err(ComputerError::CapabilityGap {
            capability: "semantic_action.uia_identity".into(),
            detail: "target element did not expose a RuntimeId".into(),
        });
    }
    let root = automation
        .ElementFromHandle(hwnd)
        .map_err(|error| uia_action_error("ElementFromHandle", error))?;
    let walker = automation
        .ControlViewWalker()
        .map_err(|error| uia_action_error("ControlViewWalker", error))?;
    let mut budget = 4096usize;
    let mut visited = HashSet::new();
    find_runtime_element(&walker, root, target_runtime_id, &mut budget, &mut visited)?.ok_or_else(
        || ComputerError::InvalidWindow("the semantic element is no longer available".into()),
    )
}

unsafe fn find_runtime_element(
    walker: &IUIAutomationTreeWalker,
    element: IUIAutomationElement,
    target_runtime_id: &[i32],
    budget: &mut usize,
    visited: &mut HashSet<Vec<i32>>,
) -> Result<Option<IUIAutomationElement>, ComputerError> {
    if *budget == 0 {
        return Ok(None);
    }
    *budget -= 1;
    let current_runtime_id = runtime_id(&element).unwrap_or_default();
    if current_runtime_id.as_slice() == target_runtime_id {
        return Ok(Some(element));
    }
    if !current_runtime_id.is_empty() && !visited.insert(current_runtime_id) {
        return Ok(None);
    }
    let mut child = first_child(walker, &element)?;
    while let Some(child_element) = child {
        let current_child = child_element.clone();
        if let Some(found) =
            find_runtime_element(walker, child_element, target_runtime_id, budget, visited)?
        {
            return Ok(Some(found));
        }
        child = next_sibling(walker, &current_child)?;
    }
    Ok(None)
}

#[allow(clippy::too_many_arguments)]
fn observe_com(
    display: &NativeDisplayInfo,
    hwnd: windows::Win32::Foundation::HWND,
    session_id: &ComputerSessionId,
    window_id: &WindowId,
    limits: SemanticObservationLimits,
    nonce: u64,
    generation: u64,
    current: &mut HashMap<ElementId, UiaElementRecord>,
) -> Result<SemanticObservation, ComputerError> {
    let uia_init_started = Instant::now();
    let apartment = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
    let uia_init_micros = uia_init_started.elapsed().as_micros();
    if apartment.0 < 0 {
        return Err(ComputerError::CapabilityGap {
            capability: "semantic_observation.uia".into(),
            detail: format!(
                "COM apartment initialization failed: hresult=0x{:08x}",
                apartment.0 as u32
            ),
        });
    }

    let result = (|| {
        let automation: IUIAutomation = unsafe {
            CoCreateInstance(&CLSID_CUI_AUTOMATION, None, CLSCTX_INPROC_SERVER)
                .map_err(|error| uia_error("CoCreateInstance(CUIAutomation)", error))?
        };
        let root = unsafe {
            automation
                .ElementFromHandle(hwnd)
                .map_err(|error| uia_error("ElementFromHandle", error))?
        };
        let walker = unsafe {
            automation
                .ControlViewWalker()
                .map_err(|error| uia_error("ControlViewWalker", error))?
        };
        let tree_started = Instant::now();
        let mut nodes = Vec::new();
        let mut truncated = false;
        let mut property_read_micros = 0u128;
        unsafe {
            visit_element(
                &walker,
                display,
                root,
                None,
                0,
                limits,
                nonce,
                generation,
                &mut nodes,
                &mut truncated,
                &mut property_read_micros,
            )?
        };
        let tree_walk_micros = tree_started.elapsed().as_micros();
        let root_element_id = nodes
            .first()
            .map(|node| node.semantic.id.clone())
            .ok_or_else(|| ComputerError::CapabilityGap {
                capability: "semantic_observation.uia_tree".into(),
                detail: "UIA returned no root element for the target window".into(),
            })?;
        nodes
            .iter()
            .map(|node| UiaElementRecord {
                semantic: node.semantic.clone(),
                password: node.password,
                runtime_id: unsafe { runtime_id(&node.element).unwrap_or_default() },
                hwnd: hwnd.0 as isize,
                window_id: window_id.clone(),
            })
            .for_each(|record| {
                current.insert(record.semantic.id.clone(), record);
            });
        let elements = nodes
            .into_iter()
            .map(|node| node.semantic)
            .collect::<Vec<_>>();
        let total_micros = uia_init_started.elapsed().as_micros();
        Ok(SemanticObservation {
            session_id: session_id.clone(),
            window_id: window_id.clone(),
            root_element_id,
            metadata: SemanticObservationMetadata {
                captured_at: Some(SystemTime::now()),
                generation,
                max_depth: limits.max_depth,
                max_elements: limits.max_elements,
                truncated,
                uia_init_micros,
                tree_walk_micros,
                property_read_micros,
                serialization_micros: None,
                total_micros,
                element_count: elements.len(),
            },
            elements,
        })
    })();
    unsafe { CoUninitialize() };
    result
}

#[allow(clippy::too_many_arguments)]
unsafe fn visit_element(
    walker: &IUIAutomationTreeWalker,
    display: &NativeDisplayInfo,
    element: IUIAutomationElement,
    parent_id: Option<ElementId>,
    depth: u32,
    limits: SemanticObservationLimits,
    nonce: u64,
    generation: u64,
    nodes: &mut Vec<TreeNode>,
    truncated: &mut bool,
    property_read_micros: &mut u128,
) -> Result<Option<usize>, ComputerError> {
    if nodes.len() >= limits.max_elements as usize {
        *truncated = true;
        return Ok(None);
    }

    let index = nodes.len();
    let id = ElementId::new(format!("uia-{nonce:016x}-{generation}-{index}"));
    let (semantic, password) = read_element(
        &element,
        display,
        id.clone(),
        parent_id,
        property_read_micros,
    )?;
    nodes.push(TreeNode {
        element,
        semantic,
        password,
    });

    if depth >= limits.max_depth {
        if first_child(walker, &nodes[index].element)?.is_some() {
            *truncated = true;
        }
        return Ok(Some(index));
    }

    let mut child = first_child(walker, &nodes[index].element)?;
    let mut child_ids = Vec::new();
    while let Some(child_element) = child {
        if nodes.len() >= limits.max_elements as usize {
            *truncated = true;
            break;
        }
        let current_child = child_element.clone();
        if let Some(child_index) = visit_element(
            walker,
            display,
            child_element,
            Some(nodes[index].semantic.id.clone()),
            depth + 1,
            limits,
            nonce,
            generation,
            nodes,
            truncated,
            property_read_micros,
        )? {
            child_ids.push(nodes[child_index].semantic.id.clone());
        }
        child = next_sibling(walker, &current_child)?;
    }
    nodes[index].semantic.child_ids = child_ids;
    Ok(Some(index))
}

unsafe fn first_child(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
) -> Result<Option<IUIAutomationElement>, ComputerError> {
    match walker.GetFirstChildElement(element) {
        Ok(child) => Ok(Some(child)),
        Err(error) if is_element_unavailable(&error) => Ok(None),
        Err(error) => Err(uia_error("GetFirstChildElement", error)),
    }
}

unsafe fn next_sibling(
    walker: &IUIAutomationTreeWalker,
    element: &IUIAutomationElement,
) -> Result<Option<IUIAutomationElement>, ComputerError> {
    match walker.GetNextSiblingElement(element) {
        Ok(sibling) => Ok(Some(sibling)),
        Err(error) if is_element_unavailable(&error) => Ok(None),
        Err(error) => Err(uia_error("GetNextSiblingElement", error)),
    }
}

#[allow(non_upper_case_globals)]
unsafe fn read_element(
    element: &IUIAutomationElement,
    display: &NativeDisplayInfo,
    id: ElementId,
    parent_id: Option<ElementId>,
    property_read_micros: &mut u128,
) -> Result<(ComputerElement, bool), ComputerError> {
    let started = Instant::now();
    let control_type = element
        .CurrentControlType()
        .map_err(|error| uia_error("CurrentControlType", error))?;
    let control_type_name = control_type_name(control_type.0);
    let role = read_bstr(
        || element.CurrentLocalizedControlType(),
        MAX_PROPERTY_TEXT_UTF16,
    )
    .unwrap_or_else(|| control_type_name.clone());
    let name = read_bstr(|| element.CurrentName(), MAX_PROPERTY_TEXT_UTF16);
    let automation_id = read_bstr(|| element.CurrentAutomationId(), MAX_PROPERTY_TEXT_UTF16);
    let class_name = read_bstr(|| element.CurrentClassName(), MAX_PROPERTY_TEXT_UTF16);
    let password = element
        .CurrentIsPassword()
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let enabled = element
        .CurrentIsEnabled()
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let focused = element
        .CurrentHasKeyboardFocus()
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let focusable = element
        .CurrentIsKeyboardFocusable()
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let offscreen = element
        .CurrentIsOffscreen()
        .map(|value| value.as_bool())
        .unwrap_or(false);
    let bounds = element
        .CurrentBoundingRectangle()
        .ok()
        .and_then(|rect| coordinate_from_rect(rect, display));

    let invoke = element
        .GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        .ok();
    let value = element
        .GetCurrentPatternAs::<IUIAutomationValuePattern>(UIA_ValuePatternId)
        .ok();
    let text = element
        .GetCurrentPatternAs::<IUIAutomationTextPattern>(UIA_TextPatternId)
        .ok();
    let selection_item = element
        .GetCurrentPatternAs::<IUIAutomationSelectionItemPattern>(UIA_SelectionItemPatternId)
        .ok();
    let expand_collapse = element
        .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(UIA_ExpandCollapsePatternId)
        .ok();
    let scroll = element
        .GetCurrentPatternAs::<IUIAutomationScrollPattern>(UIA_ScrollPatternId)
        .ok();
    let scroll_item = element
        .GetCurrentPatternAs::<IUIAutomationScrollItemPattern>(UIA_ScrollItemPatternId)
        .ok();
    let toggle = element
        .GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
        .ok();
    let range_value = element
        .GetCurrentPatternAs::<IUIAutomationRangeValuePattern>(UIA_RangeValuePatternId)
        .ok();

    let toggle_state = toggle.as_ref().and_then(|pattern| unsafe {
        match pattern.CurrentToggleState().ok()? {
            ToggleState_On => Some(true),
            ToggleState_Off => Some(false),
            _ => None,
        }
    });
    let selected = selection_item.as_ref().and_then(|pattern| unsafe {
        pattern
            .CurrentIsSelected()
            .ok()
            .map(|value| value.as_bool())
    });
    let expanded = expand_collapse.as_ref().and_then(|pattern| unsafe {
        match pattern.CurrentExpandCollapseState().ok()? {
            ExpandCollapseState_Expanded | ExpandCollapseState_PartiallyExpanded => Some(true),
            ExpandCollapseState_Collapsed => Some(false),
            _ => None,
        }
    });
    let range_value_current = range_value
        .as_ref()
        .and_then(|pattern| unsafe { pattern.CurrentValue().ok() });
    let range_minimum = range_value
        .as_ref()
        .and_then(|pattern| unsafe { pattern.CurrentMinimum().ok() });
    let range_maximum = range_value
        .as_ref()
        .and_then(|pattern| unsafe { pattern.CurrentMaximum().ok() });

    let value_summary = if password {
        None
    } else {
        value
            .as_ref()
            .and_then(|pattern| pattern.CurrentValue().ok())
            .map(|value| limit_utf16(&value))
    };
    let textual_control = [
        UIA_DocumentControlTypeId.0,
        UIA_EditControlTypeId.0,
        UIA_TextControlTypeId.0,
    ]
    .contains(&control_type.0);
    let text_summary = if password || !textual_control {
        None
    } else {
        text.as_ref()
            .and_then(|pattern| pattern.DocumentRange().ok())
            .and_then(|range| range.GetText(MAX_TEXT_SUMMARY_UTF16 as i32).ok())
            .map(|value| limit_utf16(&value))
    };

    *property_read_micros = property_read_micros.saturating_add(started.elapsed().as_micros());
    Ok((
        ComputerElement {
            id,
            parent_id,
            child_ids: Vec::new(),
            role,
            control_type: control_type_name,
            name,
            automation_id,
            class_name,
            value_summary,
            text_summary,
            bounds,
            enabled,
            focused,
            focusable,
            offscreen,
            toggle_state,
            selected,
            expanded,
            range_value: range_value_current,
            range_minimum,
            range_maximum,
            capabilities: SemanticCapabilities {
                invokable: invoke.is_some(),
                editable: value.is_some() || text.is_some(),
                selectable: selection_item.is_some(),
                scrollable: scroll.is_some() || scroll_item.is_some(),
                expandable: expand_collapse.is_some(),
                toggleable: toggle.is_some(),
                range_adjustable: range_value.is_some(),
                scroll_into_view: scroll_item.is_some(),
            },
        },
        password,
    ))
}

fn coordinate_from_rect(rect: RECT, _display: &NativeDisplayInfo) -> Option<Coordinate> {
    let width = (rect.right - rect.left).max(0) as f64;
    let height = (rect.bottom - rect.top).max(0) as f64;
    (width > 0.0 && height > 0.0).then_some(Coordinate {
        space: CoordinateSpace::DesktopPhysical,
        point: Point {
            x: rect.left as f64,
            y: rect.top as f64,
        },
        extent: Size { width, height },
        dpi: alice_computer_use_core::DpiScale::ONE,
        display_id: None,
        frame_id: None,
    })
}

fn read_bstr<F>(read: F, max_utf16: usize) -> Option<String>
where
    F: FnOnce() -> windows::core::Result<BSTR>,
{
    read()
        .ok()
        .map(|value| limit_utf16_with_limit(&value, max_utf16))
}

fn limit_utf16(value: &BSTR) -> String {
    limit_utf16_with_limit(value, MAX_TEXT_SUMMARY_UTF16)
}

fn limit_utf16_with_limit(value: &BSTR, max_utf16: usize) -> String {
    String::from_utf16_lossy(&value.as_wide()[..value.len().min(max_utf16)])
}

fn control_type_name(value: i32) -> String {
    let name = match value {
        value if value == UIA_ButtonControlTypeId.0 => "button",
        value if value == UIA_CheckBoxControlTypeId.0 => "check_box",
        value if value == UIA_ComboBoxControlTypeId.0 => "combo_box",
        value if value == UIA_CustomControlTypeId.0 => "custom",
        value if value == UIA_DocumentControlTypeId.0 => "document",
        value if value == UIA_EditControlTypeId.0 => "edit",
        value if value == UIA_ListControlTypeId.0 => "list",
        value if value == UIA_ListItemControlTypeId.0 => "list_item",
        value if value == UIA_MenuBarControlTypeId.0 => "menu_bar",
        value if value == UIA_MenuControlTypeId.0 => "menu",
        value if value == UIA_MenuItemControlTypeId.0 => "menu_item",
        value if value == UIA_PaneControlTypeId.0 => "pane",
        value if value == UIA_RadioButtonControlTypeId.0 => "radio_button",
        value if value == UIA_SliderControlTypeId.0 => "slider",
        value if value == UIA_TabControlTypeId.0 => "tab",
        value if value == UIA_TabItemControlTypeId.0 => "tab_item",
        value if value == UIA_TextControlTypeId.0 => "text",
        value if value == UIA_TreeControlTypeId.0 => "tree",
        value if value == UIA_TreeItemControlTypeId.0 => "tree_item",
        value if value == UIA_WindowControlTypeId.0 => "window",
        _ => return format!("control_type_{value}"),
    };
    name.into()
}

fn is_element_unavailable(error: &windows::core::Error) -> bool {
    // windows-rs represents a null `IUIAutomationElement**` returned with
    // S_OK as an Error whose code is zero. UIA also uses
    // UIA_E_ELEMENTNOTAVAILABLE for the same navigation boundary on some
    // providers. Both mean “there is no child/sibling”, not a failed tree.
    error.code().0 == 0 || error.code().0 as u32 == UIA_E_ELEMENT_NOT_AVAILABLE
}

fn uia_error(operation: &str, error: windows::core::Error) -> ComputerError {
    ComputerError::CapabilityGap {
        capability: "semantic_observation.uia".into(),
        detail: format!(
            "{operation} failed: {error}; hresult=0x{:08x}",
            error.code().0 as u32
        ),
    }
}

fn uia_action_error(operation: &str, error: windows::core::Error) -> ComputerError {
    ComputerError::CapabilityGap {
        capability: "semantic_action.uia".into(),
        detail: format!(
            "{operation} failed: {error}; hresult=0x{:08x}",
            error.code().0 as u32
        ),
    }
}

#[cfg(test)]
mod value_verification_tests {
    use alice_computer_use_core::{ElementId, SemanticAction, SemanticActionStatus};

    use super::{
        semantic_action_preserves_generation, text_value_matches, toggle_transition_proven,
    };

    #[test]
    fn value_readback_accepts_only_exact_text_or_platform_newline_normalization() {
        assert!(text_value_matches("hello", "hello"));
        assert!(text_value_matches("first\r\nsecond", "first\nsecond"));
        assert!(!text_value_matches("hello!", "hello"));
        assert!(!text_value_matches("prefix hello suffix", "hello"));
    }

    #[test]
    fn verified_focus_preserves_generation_but_uncertain_focus_does_not() {
        let focus = SemanticAction::Focus {
            element_id: ElementId::new("edit"),
        };
        assert!(semantic_action_preserves_generation(
            &focus,
            SemanticActionStatus::Performed,
            true,
        ));
        assert!(!semantic_action_preserves_generation(
            &focus,
            SemanticActionStatus::VerificationFailed,
            false,
        ));
    }

    #[test]
    fn toggle_accepts_a_changed_acquired_pattern_only_when_fresh_target_disappears() {
        assert!(toggle_transition_proven(Some(false), Some(true), None));
        assert!(!toggle_transition_proven(
            Some(false),
            Some(true),
            Some(false),
        ));
        assert!(!toggle_transition_proven(None, Some(true), None));
        assert!(!toggle_transition_proven(Some(false), Some(false), None));
    }
}
