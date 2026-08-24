//! Manual Computer-INFRA-R8 fixture.
//!
//! This is deliberately a small standard Win32 window.  It has no manifest,
//! no UAC automation, and no self-elevation.  Run the same executable once as
//! a normal user and once manually with "Run as administrator" to create the
//! two security contexts used by the R8 gates.

#![cfg(windows)]

use windows::{
    core::{w, Error, PCWSTR},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::HBRUSH,
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetDlgItem, GetMessageW,
            GetWindowTextLengthW, GetWindowTextW, RegisterClassW, SetWindowTextW, ShowWindow,
            TranslateMessage, BS_PUSHBUTTON, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, EN_CHANGE,
            HMENU, MSG, SW_SHOW, WINDOW_STYLE, WM_COMMAND, WM_DESTROY, WNDCLASSW, WS_BORDER,
            WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        },
    },
};

const CLASS_NAME: PCWSTR = w!("AliceComputerR8IntegrityFixture");
const WINDOW_TITLE: PCWSTR = w!("Alice Computer R8 Integrity Fixture");
const EDIT_ID: usize = 1001;
const BUTTON_ID: usize = 1002;
const STATUS_ID: usize = 1003;

unsafe fn set_status(parent: HWND, text: PCWSTR) {
    if let Ok(status) = GetDlgItem(parent, STATUS_ID as i32) {
        let _ = SetWindowTextW(status, text);
    }
}

unsafe fn update_title(parent: HWND) {
    let Ok(edit) = GetDlgItem(parent, EDIT_ID as i32) else {
        return;
    };
    let length = GetWindowTextLengthW(edit).max(0) as usize;
    let mut buffer = vec![0u16; length.saturating_add(1).max(1)];
    let copied = GetWindowTextW(edit, &mut buffer).max(0) as usize;
    let text = String::from_utf16_lossy(&buffer[..copied.min(buffer.len())])
        .replace('\r', "\\r")
        .replace('\n', "\\n");
    let title = format!("Alice Computer R8 Integrity Fixture | TEXT={text}");
    let wide = title
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let _ = SetWindowTextW(parent, PCWSTR(wide.as_ptr()));
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
                && (wparam.0 & 0xffff) == EDIT_ID =>
        {
            update_title(hwnd);
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xffff) == BUTTON_ID => {
            set_status(hwnd, w!("BUTTON_CLICKED"));
            LRESULT(0)
        }
        WM_DESTROY => {
            windows::Win32::UI::WindowsAndMessaging::PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn main() -> Result<(), Error> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hbrBackground: HBRUSH::default(),
            lpszClassName: CLASS_NAME,
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(Error::from_win32());
        }
        let window = CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            WINDOW_TITLE,
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            360,
            None,
            None,
            instance,
            None,
        )?;
        let _edit = CreateWindowExW(
            WS_EX_CLIENTEDGE,
            w!("EDIT"),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_BORDER | WS_TABSTOP,
            20,
            20,
            700,
            190,
            window,
            HMENU(EDIT_ID as *mut _),
            instance,
            None,
        )?;
        let _button = CreateWindowExW(
            Default::default(),
            w!("BUTTON"),
            w!("Safe test button"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_PUSHBUTTON as u32),
            20,
            230,
            180,
            32,
            window,
            HMENU(BUTTON_ID as *mut _),
            instance,
            None,
        )?;
        let _status = CreateWindowExW(
            Default::default(),
            w!("STATIC"),
            w!("READY"),
            WS_CHILD | WS_VISIBLE,
            220,
            236,
            300,
            24,
            window,
            HMENU(STATUS_ID as *mut _),
            instance,
            None,
        )?;
        let _ = ShowWindow(window, SW_SHOW);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
