//! Deterministic pixel-only compatibility fixture for Computer-INFRA-R9.
//!
//! This is an E2E fixture, not a production backend. It deliberately exposes
//! no child controls or semantic action provider; the visible surface exists
//! only to prove that pixel observation remains available when semantic
//! actions are not.

#![cfg(windows)]

use windows::{
    core::{w, Error, PCWSTR},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            BeginPaint, EndPaint, FillRect, GetSysColorBrush, PAINTSTRUCT, SYS_COLOR_INDEX,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, PostQuitMessage,
            RegisterClassW, ShowWindow, TranslateMessage, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
            MSG, SW_SHOW, WINDOW_EX_STYLE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
        },
    },
};

const CLASS_NAME: PCWSTR = w!("AliceComputerR9PixelOnlyFixture");
const WINDOW_TITLE: PCWSTR = w!("Alice Computer R9 Pixel-Only Fixture");

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        windows::Win32::UI::WindowsAndMessaging::WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut paint);
            let mut client = RECT::default();
            windows::Win32::UI::WindowsAndMessaging::GetClientRect(hwnd, &mut client)
                .expect("GetClientRect");
            let _ = FillRect(hdc, &client, GetSysColorBrush(SYS_COLOR_INDEX(5)));
            EndPaint(hwnd, &paint).expect("EndPaint");
            LRESULT(0)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

fn main() -> Result<(), Error> {
    unsafe {
        let module = GetModuleHandleW(None)?;
        let instance = HINSTANCE(module.0);
        RegisterClassW(&WNDCLASSW {
            hInstance: instance,
            lpszClassName: CLASS_NAME,
            lpfnWndProc: Some(window_proc),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS_NAME,
            WINDOW_TITLE,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            720,
            440,
            None,
            None,
            instance,
            None,
        )?;
        let _ = ShowWindow(hwnd, SW_SHOW);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
