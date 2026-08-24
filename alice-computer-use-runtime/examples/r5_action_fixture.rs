//! Deterministic Win32/UIA fixture for Computer-INFRA-R5.
//!
//! This executable is an E2E fixture only. It is intentionally made from
//! standard Win32 controls so the production runtime can be tested against
//! real Toggle, SelectionItem, ExpandCollapse, RangeValue, and ScrollItem
//! providers without adding fixture-specific code to the backend.

#![cfg(windows)]

use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Mutex, OnceLock,
    },
    time::Instant,
};
use windows::{
    core::{w, Error, PCWSTR},
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{UpdateWindow, HBRUSH},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::{
                InitCommonControlsEx, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX, TBM_SETPOS,
                TBM_SETRANGE, TBS_AUTOTICKS,
            },
            Input::KeyboardAndMouse::{GetKeyState, SetFocus, VK_CONTROL, VK_MENU, VK_SHIFT},
            WindowsAndMessaging::{
                CallWindowProcW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetClassNameW,
                GetDlgItem, GetMessageW, GetParent, GetWindowLongPtrW, RegisterClassW,
                SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowTextW,
                ShowWindow, TranslateMessage, BS_AUTOCHECKBOX, BS_AUTORADIOBUTTON,
                CBS_DROPDOWNLIST, CB_ADDSTRING, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW,
                CW_USEDEFAULT, ES_AUTOVSCROLL, ES_MULTILINE, GWLP_WNDPROC, GWL_STYLE, HMENU,
                LBS_NOTIFY, LB_ADDSTRING, MSG, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND,
                WM_DESTROY, WM_KEYDOWN, WM_KEYUP, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
                WM_MBUTTONDOWN, WM_MBUTTONUP, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_TIMER, WNDCLASSW,
                WNDPROC, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
                WS_VISIBLE, WS_VSCROLL,
            },
        },
    },
};

const WINDOW_CLASS: PCWSTR = w!("AliceComputerR5FixtureWindow");
const WINDOW_TITLE: PCWSTR = w!("Alice Computer R5 Fixture");
const EDIT_ID: usize = 1001;
const CHECKBOX_ID: usize = 1002;
const RADIO_A_ID: usize = 1003;
const RADIO_B_ID: usize = 1004;
const COMBO_ID: usize = 1005;
const SLIDER_ID: usize = 1006;
const LIST_ID: usize = 1007;
const MOUSE_ID: usize = 1008;
const LOG_ID: usize = 1009;

static LOG: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
static ORIGINAL_EDIT: OnceLock<WNDPROC> = OnceLock::new();
static ORIGINAL_MOUSE: OnceLock<WNDPROC> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static CHECKBOX_BN_CLICKED: AtomicU32 = AtomicU32::new(0);
static CHECKBOX_WM_COMMAND: AtomicU32 = AtomicU32::new(0);

fn log_snapshot() -> Vec<String> {
    LOG.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("R5 fixture log mutex poisoned")
        .clone()
}

fn push_log(event: String, parent: HWND) {
    LOG.get_or_init(|| Mutex::new(Vec::new()))
        .lock()
        .expect("R5 fixture log mutex poisoned")
        .push(event.clone());
    println!("logger_event={event}");
    unsafe { update_log(parent) };
}

unsafe fn update_log(parent: HWND) {
    let text = log_snapshot().join(";");
    if let Ok(log) = GetDlgItem(parent, LOG_ID as i32) {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _ = SetWindowTextW(log, PCWSTR(wide.as_ptr()));
    }
    let title = format!(
        "Alice Computer R5 Fixture | MODIFIERS={}; CHECKBOX_DIAG={}; {}",
        modifier_state(),
        checkbox_diagnostics(parent),
        text
    );
    let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
    let _ = SetWindowTextW(parent, PCWSTR(wide.as_ptr()));
}

unsafe fn checkbox_diagnostics(parent: HWND) -> String {
    let Ok(checkbox) = GetDlgItem(parent, CHECKBOX_ID as i32) else {
        return "hwnd=0;class=<missing>;style=0x0;bm_getcheck=-1;bn_clicked=0;wm_command=0".into();
    };
    let mut class_name = [0u16; 64];
    let class_len = GetClassNameW(checkbox, &mut class_name).max(0) as usize;
    let class = String::from_utf16_lossy(&class_name[..class_len]);
    let style = GetWindowLongPtrW(checkbox, GWL_STYLE) as u32;
    let check = SendMessageW(checkbox, 0x00F0, WPARAM(0), LPARAM(0)).0;
    format!(
        "hwnd=0x{:X};class={class};style=0x{style:08X};bm_getcheck={check};bn_clicked={};wm_command={}",
        checkbox.0 as usize,
        CHECKBOX_BN_CLICKED.load(Ordering::Relaxed),
        CHECKBOX_WM_COMMAND.load(Ordering::Relaxed),
    )
}

fn modifier_state() -> String {
    unsafe {
        format!(
            "ctrl={};shift={};alt={}",
            (GetKeyState(VK_CONTROL.0 as i32) as i32 & 0x8000) != 0,
            (GetKeyState(VK_SHIFT.0 as i32) as i32 & 0x8000) != 0,
            (GetKeyState(VK_MENU.0 as i32) as i32 & 0x8000) != 0
        )
    }
}

fn key_name(value: usize, lparam: isize) -> String {
    // The active Chinese IME may translate a physical letter WM_KEYDOWN to
    // VK_PROCESSKEY (0xE5). Preserve the physical key from the message's
    // scan-code bits instead of treating that as a different injected key.
    if value as u16 == 0xE5 {
        let scan_code = ((lparam as u64 >> 16) & 0xff) as u16;
        if scan_code == 0x1E {
            return "A".into();
        }
    }
    match value as u16 {
        value if value == VK_CONTROL.0 => "VK_CONTROL".into(),
        value if value == VK_SHIFT.0 => "VK_SHIFT".into(),
        value if value == VK_MENU.0 => "VK_MENU".into(),
        0x41 => "A".into(),
        0x43 => "C".into(),
        0x56 => "V".into(),
        0x5a => "Z".into(),
        0x70 => "F1".into(),
        other => format!("VK_{other:02X}"),
    }
}

unsafe extern "system" fn edit_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if matches!(message, WM_KEYDOWN | WM_KEYUP) {
        let phase = if message == WM_KEYDOWN { "down" } else { "up" };
        if let Ok(parent) = GetParent(hwnd) {
            let elapsed_ms = STARTED.get_or_init(Instant::now).elapsed().as_millis();
            push_log(
                format!(
                    "key={}:{}:{}:t={elapsed_ms}",
                    key_name(wparam.0, lparam.0),
                    phase,
                    modifier_state()
                ),
                parent,
            );
        }
    }
    let original = ORIGINAL_EDIT.get().copied().flatten();
    match original {
        Some(original) => CallWindowProcW(Some(original), hwnd, message, wparam, lparam),
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe extern "system" fn mouse_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let (button, phase) = match message {
        WM_LBUTTONDOWN => (Some("left"), Some("down")),
        WM_LBUTTONDBLCLK => (Some("left"), Some("double")),
        WM_LBUTTONUP => (Some("left"), Some("up")),
        WM_MBUTTONDOWN => (Some("middle"), Some("down")),
        WM_MBUTTONUP => (Some("middle"), Some("up")),
        WM_RBUTTONDOWN => (Some("right"), Some("down")),
        WM_RBUTTONUP => (Some("right"), Some("up")),
        _ => (None, None),
    };
    if let (Some(button), Some(phase)) = (button, phase) {
        if let Ok(parent) = GetParent(hwnd) {
            let count = log_snapshot()
                .iter()
                .filter(|event| event.starts_with(&format!("mouse={button}:down")))
                .count()
                + usize::from(phase == "down");
            push_log(
                format!(
                    "mouse={button}:{phase}:count={count}:{}:wparam=0x{:X}",
                    modifier_state(),
                    wparam.0
                ),
                parent,
            );
        }
    }
    let original = ORIGINAL_MOUSE.get().copied().flatten();
    match original {
        Some(original) => CallWindowProcW(Some(original), hwnd, message, wparam, lparam),
        None => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn create_child(
    class: PCWSTR,
    title: PCWSTR,
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    parent: HWND,
    id: usize,
    instance: HINSTANCE,
) -> Result<HWND, Error> {
    CreateWindowExW(
        ex_style,
        class,
        title,
        style,
        x,
        y,
        width,
        height,
        parent,
        HMENU(id as *mut _),
        instance,
        None,
    )
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_TIMER => {
            update_log(hwnd);
            LRESULT(0)
        }
        WM_COMMAND if (wparam.0 & 0xffff) == CHECKBOX_ID => {
            CHECKBOX_WM_COMMAND.fetch_add(1, Ordering::Relaxed);
            if ((wparam.0 >> 16) & 0xffff) == 0 {
                CHECKBOX_BN_CLICKED.fetch_add(1, Ordering::Relaxed);
            }
            if let Ok(checkbox) = GetDlgItem(hwnd, CHECKBOX_ID as i32) {
                let current = SendMessageW(checkbox, 0x00F0, WPARAM(0), LPARAM(0)).0;
                println!(
                    "toggle_command current_check={current} bn_clicked={} wm_command={}",
                    CHECKBOX_BN_CLICKED.load(Ordering::Relaxed),
                    CHECKBOX_WM_COMMAND.load(Ordering::Relaxed),
                );
            }
            if std::env::var_os("ALICE_R5_SLOW_TOGGLE").is_some() {
                // The standard checkbox has already changed its state before
                // the parent notification is delivered. Killing the adapter
                // during this bounded delay therefore creates an uncertain
                // response without replaying the Toggle provider call.
                std::thread::sleep(std::time::Duration::from_millis(1000));
            }
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
        let common = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES,
        };
        let _ = InitCommonControlsEx(&common);
        RegisterClassW(&WNDCLASSW {
            hCursor: windows::Win32::UI::WindowsAndMessaging::LoadCursorW(
                None,
                windows::Win32::UI::WindowsAndMessaging::IDC_ARROW,
            )?,
            hInstance: instance,
            lpszClassName: WINDOW_CLASS,
            lpfnWndProc: Some(window_proc),
            hbrBackground: HBRUSH::default(),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        });
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            WINDOW_TITLE,
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            760,
            560,
            None,
            None,
            instance,
            None,
        )?;

        let edit = create_child(
            w!("EDIT"),
            w!("R5 input logger"),
            WS_CHILD
                | WS_VISIBLE
                | WS_BORDER
                | WS_TABSTOP
                | WINDOW_STYLE((ES_MULTILINE | ES_AUTOVSCROLL) as u32),
            WINDOW_EX_STYLE(WS_EX_CLIENTEDGE.0),
            20,
            20,
            330,
            60,
            hwnd,
            EDIT_ID,
            instance,
        )?;
        let _ = ORIGINAL_EDIT.set(std::mem::transmute::<isize, WNDPROC>(SetWindowLongPtrW(
            edit,
            GWLP_WNDPROC,
            edit_proc as *const () as isize,
        )));

        let _checkbox = create_child(
            w!("BUTTON"),
            w!("R5 CheckBox"),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE((BS_AUTOCHECKBOX | 0x00004000) as u32),
            WINDOW_EX_STYLE(0),
            380,
            20,
            160,
            28,
            hwnd,
            CHECKBOX_ID,
            instance,
        )?;
        let radio_a = create_child(
            w!("BUTTON"),
            w!("R5 Radio A"),
            WS_CHILD
                | WS_VISIBLE
                | WS_TABSTOP
                | WINDOW_STYLE((BS_AUTORADIOBUTTON | 0x00000400) as u32),
            WINDOW_EX_STYLE(0),
            380,
            55,
            160,
            28,
            hwnd,
            RADIO_A_ID,
            instance,
        )?;
        let _ = SendMessageW(radio_a, 0x00F1, WPARAM(1), LPARAM(0));
        let _radio_b = create_child(
            w!("BUTTON"),
            w!("R5 Radio B"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(BS_AUTORADIOBUTTON as u32),
            WINDOW_EX_STYLE(0),
            380,
            90,
            160,
            28,
            hwnd,
            RADIO_B_ID,
            instance,
        )?;

        let combo = create_child(
            w!("COMBOBOX"),
            w!("R5 Combo"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
            WINDOW_EX_STYLE(0),
            20,
            105,
            220,
            120,
            hwnd,
            COMBO_ID,
            instance,
        )?;
        for title in [w!("R5 Combo One"), w!("R5 Combo Two")] {
            let _ = SendMessageW(combo, CB_ADDSTRING, WPARAM(0), LPARAM(title.0 as isize));
        }
        let _ = SendMessageW(combo, CB_SETCURSEL, WPARAM(0), LPARAM(0));

        let slider = create_child(
            w!("msctls_trackbar32"),
            w!("R5 Slider"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP | WINDOW_STYLE(TBS_AUTOTICKS),
            WINDOW_EX_STYLE(0),
            20,
            245,
            330,
            45,
            hwnd,
            SLIDER_ID,
            instance,
        )?;
        let _ = SendMessageW(
            slider,
            TBM_SETRANGE,
            WPARAM(1),
            LPARAM((100i32 << 16) as isize),
        );
        let _ = SendMessageW(slider, TBM_SETPOS, WPARAM(1), LPARAM(50));

        let list = create_child(
            w!("LISTBOX"),
            w!("R5 Scroll List"),
            WS_CHILD
                | WS_VISIBLE
                | WS_BORDER
                | WS_TABSTOP
                | WS_VSCROLL
                | WINDOW_STYLE(LBS_NOTIFY as u32),
            WINDOW_EX_STYLE(0),
            380,
            135,
            250,
            130,
            hwnd,
            LIST_ID,
            instance,
        )?;
        for index in 1..=40 {
            let title = format!("R5 Item {index:02}");
            let wide: Vec<u16> = title.encode_utf16().chain(std::iter::once(0)).collect();
            let _ = SendMessageW(
                list,
                LB_ADDSTRING,
                WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
            );
        }

        let mouse = create_child(
            w!("BUTTON"),
            w!("R5 Mouse Target"),
            WS_CHILD | WS_VISIBLE | WS_TABSTOP,
            WINDOW_EX_STYLE(0),
            20,
            310,
            220,
            50,
            hwnd,
            MOUSE_ID,
            instance,
        )?;
        let _ = ORIGINAL_MOUSE.set(std::mem::transmute::<isize, WNDPROC>(SetWindowLongPtrW(
            mouse,
            GWLP_WNDPROC,
            mouse_proc as *const () as isize,
        )));

        create_child(
            w!("STATIC"),
            w!("R5 logger output"),
            WS_CHILD | WS_VISIBLE | WS_BORDER,
            WINDOW_EX_STYLE(0),
            20,
            390,
            680,
            90,
            hwnd,
            LOG_ID,
            instance,
        )?;

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(edit);
        let _ = SetTimer(hwnd, 1, 50, None);
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).into() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}
