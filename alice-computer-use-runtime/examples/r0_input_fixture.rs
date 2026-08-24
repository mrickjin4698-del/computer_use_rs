//! Deterministic Win32 fixture for Computer-INFRA-R0/R2/R3 testing.
//!
//! This is an E2E test fixture, not a production ComputerBackend. It creates a
//! normal top-level Win32 window with one standard EDIT control and keeps it
//! alive until the user closes it.

#![cfg(windows)]

use std::sync::{
    atomic::{AtomicU32, Ordering},
    Mutex, OnceLock,
};
use windows::{
    core::{w, Error, PCWSTR},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{UpdateWindow, HBRUSH},
        System::LibraryLoader::GetModuleHandleW,
        UI::Input::KeyboardAndMouse::{
            SetFocus, VK_BACK, VK_CONTROL, VK_MENU, VK_RETURN, VK_SHIFT, VK_TAB,
        },
        UI::WindowsAndMessaging::{
            CallWindowProcW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem,
            GetMessageW, GetParent, GetWindowTextLengthW, GetWindowTextW, RegisterClassW,
            SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
            TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, EN_CHANGE, ES_AUTOVSCROLL,
            ES_MULTILINE, ES_WANTRETURN, GWLP_WNDPROC, HMENU, MSG, SW_SHOW, WINDOW_STYLE,
            WM_COMMAND, WM_DESTROY, WM_GETTEXT, WM_GETTEXTLENGTH, WM_KEYDOWN, WM_KEYUP,
            WM_LBUTTONUP, WM_SETTEXT, WNDCLASSW, WNDPROC, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE,
            WS_HSCROLL, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
        },
    },
};

const WINDOW_CLASS: PCWSTR = w!("AliceComputerInputFixtureWindow");
const WINDOW_TITLE: PCWSTR = w!("Alice Computer Input Fixture");
const EDIT_CONTROL_ID: usize = 1001;
const BUTTON_CONTROL_ID: usize = 1002;
const STATUS_CONTROL_ID: usize = 1003;
const PIXEL_TARGET_CONTROL_ID: usize = 1004;
const TITLE_PREFIX: &str = "Alice Computer Input Fixture | ";
const KEYLOG_SEPARATOR: &str = " || KEYLOG=";

static KEY_LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static ORIGINAL_EDIT_PROC: OnceLock<WNDPROC> = OnceLock::new();
static ORIGINAL_PIXEL_PROC: OnceLock<WNDPROC> = OnceLock::new();
static BUTTON_INVOKE_COUNT: AtomicU32 = AtomicU32::new(0);
static PIXEL_CLICK_COUNT: AtomicU32 = AtomicU32::new(0);

fn escaped_title_text(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '\r' => escaped.push_str("\\r"),
            '\n' => escaped.push_str("\\n"),
            _ => escaped.push(ch),
        }
    }
    escaped
}

fn escape_fragment(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('\r', "\\r")
        .replace('\n', "\\n")
        .replace('|', "\\|")
}

fn key_log_snapshot() -> Vec<String> {
    KEY_LOG
        .get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("fixture key log mutex poisoned")
        .clone()
}

fn key_name(vk: usize) -> Option<&'static str> {
    match vk as u16 {
        value if value == VK_CONTROL.0 => Some("VK_CONTROL"),
        value if value == VK_SHIFT.0 => Some("VK_SHIFT"),
        value if value == VK_MENU.0 => Some("VK_MENU"),
        value if value == VK_RETURN.0 => Some("VK_RETURN"),
        value if value == VK_TAB.0 => Some("VK_TAB"),
        value if value == VK_BACK.0 => Some("VK_BACK"),
        0x41 => Some("A"),
        0x43 => Some("C"),
        0x56 => Some("V"),
        0x5a => Some("Z"),
        _ => None,
    }
}

unsafe fn read_edit_text(edit: HWND) -> String {
    let length = SendMessageW(edit, WM_GETTEXTLENGTH, WPARAM(0), LPARAM(0))
        .0
        .max(0) as usize;
    let mut buffer = vec![0u16; length + 1];
    let copied = SendMessageW(
        edit,
        WM_GETTEXT,
        WPARAM(buffer.len()),
        LPARAM(buffer.as_mut_ptr() as isize),
    )
    .0
    .max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
}

unsafe fn update_fixture_title(hwnd: HWND) {
    let Ok(edit) = GetDlgItem(hwnd, EDIT_CONTROL_ID as i32) else {
        return;
    };
    if edit.0.is_null() {
        return;
    }
    let text = read_edit_text(edit);
    let keylog = key_log_snapshot()
        .iter()
        .map(|event| escape_fragment(event))
        .collect::<Vec<_>>()
        .join(",");
    let button_invokes = BUTTON_INVOKE_COUNT.load(Ordering::Relaxed);
    let pixel_clicks = PIXEL_CLICK_COUNT.load(Ordering::Relaxed);
    let status = GetDlgItem(hwnd, STATUS_CONTROL_ID as i32)
        .ok()
        .filter(|status| !status.0.is_null())
        .map(|status| read_window_text(status))
        .unwrap_or_default();
    let title = format!(
        "{TITLE_PREFIX}{} || STATUS={status}{KEYLOG_SEPARATOR}BUTTON_INVOKES={button_invokes};PIXEL_CLICKS={pixel_clicks};{keylog}",
        escaped_title_text(&text)
    );
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
}

unsafe fn read_window_text(hwnd: HWND) -> String {
    let length = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut buffer = vec![0u16; length.saturating_add(1).max(1)];
    let copied = GetWindowTextW(hwnd, &mut buffer).max(0) as usize;
    String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if matches!(message, WM_KEYDOWN | WM_KEYUP) {
        if let Some(name) = key_name(wparam.0) {
            let phase = if message == WM_KEYDOWN { "down" } else { "up" };
            let event = format!("{name} {phase}");
            KEY_LOG
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .expect("fixture key log mutex poisoned")
                .push(event.clone());
            println!("logger_event={event}");
            if let Ok(parent) = GetParent(hwnd) {
                update_fixture_title(parent);
            }
        }
    }

    let original = ORIGINAL_EDIT_PROC.get().copied().flatten();
    let result = match original {
        Some(original) => CallWindowProcW(Some(original), hwnd, message, wparam, lparam),
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    };
    if message == WM_SETTEXT {
        if let Ok(parent) = GetParent(hwnd) {
            update_fixture_title(parent);
        }
    }
    result
}

unsafe fn record_pixel_click(parent: HWND) {
    let count = PIXEL_CLICK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    if let Ok(status) = GetDlgItem(parent, STATUS_CONTROL_ID as i32) {
        let label = if count % 2 == 1 {
            w!("PIXEL_INVOKED")
        } else {
            w!("READY")
        };
        let _ = SetWindowTextW(status, label);
    }
    update_fixture_title(parent);
}

unsafe extern "system" fn pixel_target_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_LBUTTONUP {
        if let Ok(parent) = GetParent(hwnd) {
            record_pixel_click(parent);
        }
    }

    let original = ORIGINAL_PIXEL_PROC.get().copied().flatten();
    match original {
        Some(original) => CallWindowProcW(Some(original), hwnd, message, wparam, lparam),
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_COMMAND
            if ((wparam.0 >> 16) & 0xffff) as u32 == EN_CHANGE
                && (wparam.0 & 0xffff) == EDIT_CONTROL_ID =>
        {
            update_fixture_title(hwnd);
            return LRESULT(0);
        }
        WM_COMMAND
            if ((wparam.0 >> 16) & 0xffff) == 0 && (wparam.0 & 0xffff) == BUTTON_CONTROL_ID =>
        {
            let count = BUTTON_INVOKE_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
            if let Ok(status) = GetDlgItem(hwnd, STATUS_CONTROL_ID as i32) {
                let label = if count % 2 == 1 {
                    w!("INVOKED")
                } else {
                    w!("READY")
                };
                let _ = SetWindowTextW(status, label);
            }
            update_fixture_title(hwnd);
            if std::env::var_os("ALICE_R2_SLOW_INVOKE").is_some() {
                // Deterministic crash-gate harness delay; this fixture is not
                // production automation code and the delay is opt-in.
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            if std::env::var_os("ALICE_R3_SLOW_INVOKE").is_some() {
                // The state transition happens before this delay. Killing the
                // sidecar during the delay creates a deterministic uncertain
                // response without making the fixture action execute twice.
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
            return LRESULT(0);
        }
        WM_DESTROY => {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

fn main() -> windows::core::Result<()> {
    let module = unsafe { GetModuleHandleW(None)? };
    let instance = HINSTANCE(module.0);
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_proc),
        hInstance: instance,
        hbrBackground: HBRUSH::default(),
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err(Error::from_win32());
    }

    let window = unsafe {
        CreateWindowExW(
            Default::default(),
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            900,
            520,
            None,
            None,
            instance,
            None,
        )?
    };
    let edit = unsafe {
        CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD
                | WS_VISIBLE
                | WS_BORDER
                | WS_TABSTOP
                | WS_HSCROLL
                | WS_VSCROLL
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL | ES_WANTRETURN) as u32),
            12,
            12,
            856,
            430,
            window,
            HMENU(EDIT_CONTROL_ID as *mut _),
            instance,
            None,
        )?
    };
    let _button = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            w!("Set Status"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            12,
            458,
            150,
            32,
            window,
            HMENU(BUTTON_CONTROL_ID as *mut _),
            instance,
            None,
        )?
    };
    let _status = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("READY"),
            WS_CHILD | WS_VISIBLE,
            180,
            458,
            180,
            32,
            window,
            HMENU(STATUS_CONTROL_ID as *mut _),
            instance,
            None,
        )?
    };
    let _pixel_target = unsafe {
        CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("Pixel Only Target"),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WINDOW_STYLE(0x0100),
            390,
            458,
            180,
            32,
            window,
            HMENU(PIXEL_TARGET_CONTROL_ID as *mut _),
            instance,
            None,
        )?
    };
    unsafe {
        let previous = SetWindowLongPtrW(edit, GWLP_WNDPROC, edit_proc as *const () as isize);
        let _ = ORIGINAL_EDIT_PROC.set(std::mem::transmute::<isize, WNDPROC>(previous));
        let previous = SetWindowLongPtrW(
            _pixel_target,
            GWLP_WNDPROC,
            pixel_target_proc as *const () as isize,
        );
        let _ = ORIGINAL_PIXEL_PROC.set(std::mem::transmute::<isize, WNDPROC>(previous));
        let _ = ShowWindow(window, SW_SHOW);
        let _ = SetForegroundWindow(window);
        let _ = UpdateWindow(window);
        let _ = SetFocus(edit);
        update_fixture_title(window);
    }
    println!(
        "fixture=Alice Computer Input Fixture hwnd={} edit_hwnd={} pixel_target_id={}",
        window.0 as isize, edit.0 as isize, PIXEL_TARGET_CONTROL_ID
    );

    let mut message = MSG::default();
    loop {
        let result = unsafe { GetMessageW(&mut message, None, 0, 0).0 };
        if result == -1 {
            return Err(Error::from_win32());
        }
        if result == 0 {
            break;
        }
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
