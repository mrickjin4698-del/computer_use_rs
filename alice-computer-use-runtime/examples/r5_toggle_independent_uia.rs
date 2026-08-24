//! Independent UIA TogglePattern probe for Computer-INFRA-R5.1.
//!
//! This is a diagnostic executable only. It does not use Alice's runtime,
//! UiaStore, WinNativeBackend, or any fixture state-mutating message. It
//! invokes Toggle exactly once and reports native/UIA before/after evidence.

#![cfg(windows)]

use windows::{
    core::{w, GUID, PCWSTR},
    Win32::{
        Foundation::{HWND, LPARAM, POINT, WPARAM},
        System::Com::{
            CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
            COINIT_MULTITHREADED,
        },
        UI::Accessibility::{
            ExpandCollapseState_Collapsed, ExpandCollapseState_Expanded, IUIAutomation,
            IUIAutomationElement, IUIAutomationExpandCollapsePattern, IUIAutomationTogglePattern,
            ToggleState_Off, ToggleState_On, UIA_ExpandCollapsePatternId, UIA_TogglePatternId,
        },
        UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2},
        UI::Input::KeyboardAndMouse::{
            SendInput, INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
            MOUSEINPUT,
        },
        UI::WindowsAndMessaging::{
            FindWindowW, GetClassNameW, GetDlgItem, GetForegroundWindow, GetWindowLongPtrW,
            GetWindowRect, SendMessageW, SetCursorPos, WindowFromPoint, GWL_STYLE,
        },
    },
};

const CLSID_CUI_AUTOMATION: GUID = GUID::from_u128(0xff48dba4_60ef_4201_aa87_54103eef594e);
const CHECKBOX_ID: i32 = 1002;
const COMBO_ID: i32 = 1005;
const WINDOW_CLASS: PCWSTR = w!("AliceComputerR5FixtureWindow");
fn state_name(state: i32) -> &'static str {
    match state {
        value if value == ToggleState_On.0 => "On",
        value if value == ToggleState_Off.0 => "Off",
        _ => "Indeterminate",
    }
}

unsafe fn native_check(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, 0x00F0, WPARAM(0), LPARAM(0)).0 as i32
}

unsafe fn identity(hwnd: HWND) -> (String, u32) {
    let mut class_name = [0u16; 64];
    let length = GetClassNameW(hwnd, &mut class_name).max(0) as usize;
    (
        String::from_utf16_lossy(&class_name[..length]),
        GetWindowLongPtrW(hwnd, GWL_STYLE) as u32,
    )
}

unsafe fn class_name(hwnd: HWND) -> String {
    let mut class_name = [0u16; 64];
    let length = GetClassNameW(hwnd, &mut class_name).max(0) as usize;
    String::from_utf16_lossy(&class_name[..length])
}

unsafe fn toggle_pattern(
    element: &IUIAutomationElement,
) -> Result<IUIAutomationTogglePattern, windows::core::Error> {
    element.GetCurrentPatternAs::<IUIAutomationTogglePattern>(UIA_TogglePatternId)
}

unsafe fn toggle_state(pattern: &IUIAutomationTogglePattern) -> Result<i32, windows::core::Error> {
    Ok(pattern.CurrentToggleState()?.0)
}

unsafe fn expand_state(
    pattern: &IUIAutomationExpandCollapsePattern,
) -> Result<i32, windows::core::Error> {
    Ok(pattern.CurrentExpandCollapseState()?.0)
}

fn expand_state_name(state: i32) -> &'static str {
    match state {
        value if value == ExpandCollapseState_Expanded.0 => "Expanded",
        value if value == ExpandCollapseState_Collapsed.0 => "Collapsed",
        _ => "Other",
    }
}

unsafe fn physical_click(hwnd: HWND) -> Result<(), windows::core::Error> {
    let mut rect = windows::Win32::Foundation::RECT::default();
    GetWindowRect(hwnd, &mut rect)?;
    let x = (rect.left + rect.right) / 2;
    let y = (rect.top + rect.bottom) / 2;
    SetCursorPos(x, y)?;
    let mut cursor = POINT::default();
    windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor)?;
    let hit = WindowFromPoint(POINT { x, y });
    let events = [
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_LEFTDOWN,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_LEFTUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        },
    ];
    let sent = SendInput(&events, std::mem::size_of::<INPUT>() as i32);
    if sent as usize != events.len() {
        return Err(windows::core::Error::from_win32());
    }
    println!(
        "independent_physical_click hwnd=0x{:X} foreground=0x{:X} hit=0x{:X} hit_class={} cursor=({},{}); rect=({},{}..{},{}); sent={sent}; bm_getcheck={}",
        hwnd.0 as usize,
        GetForegroundWindow().0 as usize,
        hit.0 as usize,
        class_name(hit),
        cursor.x,
        cursor.y,
        rect.left,
        rect.top,
        rect.right,
        rect.bottom,
        native_check(hwnd),
    );
    Ok(())
}

fn main() -> Result<(), windows::core::Error> {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let top = FindWindowW(WINDOW_CLASS, PCWSTR::null())?;
        let checkbox = GetDlgItem(top, CHECKBOX_ID)?;
        let (class_name, style) = identity(checkbox);
        println!(
            "independent_identity hwnd=0x{:X} class={} style=0x{style:08X} bm_getcheck={}",
            checkbox.0 as usize,
            class_name,
            native_check(checkbox)
        );
        if std::env::args().any(|arg| arg == "--physical") {
            physical_click(checkbox)?;
            return Ok(());
        }
        if std::env::args().any(|arg| arg == "--expand") {
            let combo = GetDlgItem(top, COMBO_ID)?;
            let apartment = CoInitializeEx(None, COINIT_MULTITHREADED);
            if apartment.0 < 0 {
                return Err(windows::core::Error::from_hresult(apartment));
            }
            let result = (|| {
                let automation: IUIAutomation =
                    CoCreateInstance(&CLSID_CUI_AUTOMATION, None, CLSCTX_INPROC_SERVER)?;
                let element = automation.ElementFromHandle(combo)?;
                let pattern = element.GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                    UIA_ExpandCollapsePatternId,
                )?;
                let before = expand_state(&pattern)?;
                println!(
                    "independent_expand_before state={} pattern_id={} hwnd=0x{:X}",
                    expand_state_name(before),
                    UIA_ExpandCollapsePatternId.0,
                    combo.0 as usize
                );
                let expanded = pattern.Expand();
                println!("independent_expand_hresult={expanded:?}");
                expanded?;
                let same_after = expand_state(&pattern)?;
                let fresh_element = automation.ElementFromHandle(combo)?;
                let fresh_pattern = fresh_element
                    .GetCurrentPatternAs::<IUIAutomationExpandCollapsePattern>(
                        UIA_ExpandCollapsePatternId,
                    )?;
                let fresh_after = expand_state(&fresh_pattern)?;
                println!(
                    "independent_expand_after same={} fresh={}",
                    expand_state_name(same_after),
                    expand_state_name(fresh_after)
                );
                Ok::<(), windows::core::Error>(())
            })();
            CoUninitialize();
            return result;
        }
        println!(
            "independent_pattern_id={} interface=IUIAutomationTogglePattern",
            UIA_TogglePatternId.0
        );

        let apartment = CoInitializeEx(None, COINIT_MULTITHREADED);
        if apartment.0 < 0 {
            return Err(windows::core::Error::from_hresult(apartment));
        }
        let result = (|| {
            let automation: IUIAutomation =
                CoCreateInstance(&CLSID_CUI_AUTOMATION, None, CLSCTX_INPROC_SERVER)?;
            let element = automation.ElementFromHandle(checkbox)?;
            let pattern = toggle_pattern(&element)?;
            let before = toggle_state(&pattern)?;
            println!(
                "independent_before uia={} bm_getcheck={}",
                state_name(before),
                native_check(checkbox)
            );
            if std::env::args().any(|arg| arg == "--observe-only") {
                return Ok::<(), windows::core::Error>(());
            }

            let toggle_result = pattern.Toggle();
            println!("independent_toggle_hresult={:?}", toggle_result);
            toggle_result?;

            let same_after = toggle_state(&pattern)?;
            let fresh_element = automation.ElementFromHandle(checkbox)?;
            let fresh_pattern = toggle_pattern(&fresh_element)?;
            let fresh_after = toggle_state(&fresh_pattern)?;
            println!(
                "independent_after same_element={} fresh_element={} bm_getcheck={}",
                state_name(same_after),
                state_name(fresh_after),
                native_check(checkbox)
            );
            println!(
                "independent_transition={} same_fresh_agree={} native_matches_fresh={}",
                before != fresh_after,
                same_after == fresh_after,
                (fresh_after == ToggleState_On.0 && native_check(checkbox) == 1)
                    || (fresh_after == ToggleState_Off.0 && native_check(checkbox) == 0)
            );
            Ok::<(), windows::core::Error>(())
        })();
        CoUninitialize();
        result
    }
}
