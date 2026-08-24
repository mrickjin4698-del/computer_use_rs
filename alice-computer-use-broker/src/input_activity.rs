//! Host-owned low-level input attribution.
//!
//! The hook callback intentionally does not take a broker lock, allocate, log,
//! or call into UI/runtime code.  It only classifies the event and increments
//! bounded monotonic counters.  The broker samples those counters when it
//! admits and completes a short-lived interaction lease.

use std::sync::atomic::{AtomicBool, AtomicPtr, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

pub const ALICE_COMPUTER_INPUT_MARKER: usize =
    alice_computer_use_core::ALICE_COMPUTER_INPUT_MARKER as usize;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputEventKind {
    Keyboard,
    MouseMove,
    MouseButton,
    MouseWheel,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputSource {
    Hardware,
    AliceInjected,
    OtherInjected,
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct InputActivitySnapshot {
    pub total_sequence: u64,
    pub hardware_sequence: u64,
    pub hardware_mouse_move_sequence: u64,
    pub hardware_mouse_button_sequence: u64,
    pub hardware_mouse_wheel_sequence: u64,
    pub hardware_keyboard_sequence: u64,
    pub alice_injected_sequence: u64,
    pub other_injected_sequence: u64,
    pub other_injected_mouse_move_sequence: u64,
    pub other_injected_mouse_button_sequence: u64,
    pub other_injected_mouse_wheel_sequence: u64,
    pub other_injected_keyboard_sequence: u64,
    pub unknown_sequence: u64,
    pub monitor_available: bool,
}

struct InputActivityMonitorInner {
    expected_marker: usize,
    available: AtomicBool,
    running: AtomicBool,
    total_sequence: AtomicU64,
    hardware_sequence: AtomicU64,
    hardware_mouse_move_sequence: AtomicU64,
    hardware_mouse_button_sequence: AtomicU64,
    hardware_mouse_wheel_sequence: AtomicU64,
    hardware_keyboard_sequence: AtomicU64,
    alice_injected_sequence: AtomicU64,
    other_injected_sequence: AtomicU64,
    other_injected_mouse_move_sequence: AtomicU64,
    other_injected_mouse_button_sequence: AtomicU64,
    other_injected_mouse_wheel_sequence: AtomicU64,
    other_injected_keyboard_sequence: AtomicU64,
    unknown_sequence: AtomicU64,
    thread_id: AtomicU32,
}

impl InputActivityMonitorInner {
    fn new(expected_marker: usize) -> Self {
        Self {
            expected_marker,
            available: AtomicBool::new(false),
            running: AtomicBool::new(true),
            total_sequence: AtomicU64::new(0),
            hardware_sequence: AtomicU64::new(0),
            hardware_mouse_move_sequence: AtomicU64::new(0),
            hardware_mouse_button_sequence: AtomicU64::new(0),
            hardware_mouse_wheel_sequence: AtomicU64::new(0),
            hardware_keyboard_sequence: AtomicU64::new(0),
            alice_injected_sequence: AtomicU64::new(0),
            other_injected_sequence: AtomicU64::new(0),
            other_injected_mouse_move_sequence: AtomicU64::new(0),
            other_injected_mouse_button_sequence: AtomicU64::new(0),
            other_injected_mouse_wheel_sequence: AtomicU64::new(0),
            other_injected_keyboard_sequence: AtomicU64::new(0),
            unknown_sequence: AtomicU64::new(0),
            thread_id: AtomicU32::new(0),
        }
    }

    fn record(&self, source: InputSource, kind: InputEventKind) {
        self.total_sequence.fetch_add(1, Ordering::Relaxed);
        match source {
            InputSource::Hardware => {
                self.hardware_sequence.fetch_add(1, Ordering::Relaxed);
                match kind {
                    InputEventKind::Keyboard => {
                        self.hardware_keyboard_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseMove => {
                        self.hardware_mouse_move_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseButton => {
                        self.hardware_mouse_button_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseWheel => {
                        self.hardware_mouse_wheel_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::Unknown => {}
                }
            }
            InputSource::AliceInjected => {
                self.alice_injected_sequence.fetch_add(1, Ordering::Relaxed);
            }
            InputSource::OtherInjected => {
                self.other_injected_sequence.fetch_add(1, Ordering::Relaxed);
                match kind {
                    InputEventKind::Keyboard => {
                        self.other_injected_keyboard_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseMove => {
                        self.other_injected_mouse_move_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseButton => {
                        self.other_injected_mouse_button_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::MouseWheel => {
                        self.other_injected_mouse_wheel_sequence
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    InputEventKind::Unknown => {}
                }
            }
            InputSource::Unknown => {
                self.unknown_sequence.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self) -> InputActivitySnapshot {
        InputActivitySnapshot {
            total_sequence: self.total_sequence.load(Ordering::Acquire),
            hardware_sequence: self.hardware_sequence.load(Ordering::Acquire),
            hardware_mouse_move_sequence: self.hardware_mouse_move_sequence.load(Ordering::Acquire),
            hardware_mouse_button_sequence: self
                .hardware_mouse_button_sequence
                .load(Ordering::Acquire),
            hardware_mouse_wheel_sequence: self
                .hardware_mouse_wheel_sequence
                .load(Ordering::Acquire),
            hardware_keyboard_sequence: self.hardware_keyboard_sequence.load(Ordering::Acquire),
            alice_injected_sequence: self.alice_injected_sequence.load(Ordering::Acquire),
            other_injected_sequence: self.other_injected_sequence.load(Ordering::Acquire),
            other_injected_mouse_move_sequence: self
                .other_injected_mouse_move_sequence
                .load(Ordering::Acquire),
            other_injected_mouse_button_sequence: self
                .other_injected_mouse_button_sequence
                .load(Ordering::Acquire),
            other_injected_mouse_wheel_sequence: self
                .other_injected_mouse_wheel_sequence
                .load(Ordering::Acquire),
            other_injected_keyboard_sequence: self
                .other_injected_keyboard_sequence
                .load(Ordering::Acquire),
            unknown_sequence: self.unknown_sequence.load(Ordering::Acquire),
            monitor_available: self.available.load(Ordering::Acquire),
        }
    }
}

#[cfg(windows)]
static GLOBAL_MONITOR: AtomicPtr<InputActivityMonitorInner> = AtomicPtr::new(std::ptr::null_mut());

pub struct InputActivityMonitor {
    inner: Arc<InputActivityMonitorInner>,
    #[cfg(windows)]
    hook_thread: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl InputActivityMonitor {
    pub fn new(expected_marker: usize) -> Self {
        let inner = Arc::new(InputActivityMonitorInner::new(expected_marker));

        #[cfg(windows)]
        {
            let pointer = Arc::as_ptr(&inner) as *mut InputActivityMonitorInner;
            if GLOBAL_MONITOR
                .compare_exchange(
                    std::ptr::null_mut(),
                    pointer,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_err()
            {
                // A process owns one desktop hook monitor.  A second broker
                // must fail closed instead of silently sharing mutable state.
                inner.running.store(false, Ordering::Release);
                return Self {
                    inner,
                    hook_thread: std::sync::Mutex::new(None),
                };
            }

            let (ready_sender, ready_receiver) = std::sync::mpsc::channel();
            let thread_inner = Arc::clone(&inner);
            let mut hook_thread = Some(std::thread::spawn(move || {
                hook_thread_main(thread_inner, ready_sender)
            }));
            let installed = ready_receiver
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap_or(false);
            if !installed {
                inner.running.store(false, Ordering::Release);
                let thread_id = inner.thread_id.load(Ordering::Acquire);
                if thread_id != 0 {
                    unsafe {
                        use windows_sys::Win32::UI::WindowsAndMessaging::{
                            PostThreadMessageW, WM_QUIT,
                        };
                        let _ = PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
                    }
                }
                if let Some(thread) = hook_thread.take() {
                    let _ = thread.join();
                }
                GLOBAL_MONITOR.store(std::ptr::null_mut(), Ordering::Release);
            }
            Self {
                inner,
                hook_thread: std::sync::Mutex::new(hook_thread),
            }
        }

        #[cfg(not(windows))]
        {
            inner.available.store(true, Ordering::Release);
            Self { inner }
        }
    }

    pub fn available(&self) -> bool {
        self.inner.available.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> InputActivitySnapshot {
        self.inner.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn for_test(available: bool) -> Self {
        let inner = Arc::new(InputActivityMonitorInner::new(ALICE_COMPUTER_INPUT_MARKER));
        inner.available.store(available, Ordering::Release);
        #[cfg(windows)]
        {
            Self {
                inner,
                hook_thread: std::sync::Mutex::new(None),
            }
        }
        #[cfg(not(windows))]
        {
            Self { inner }
        }
    }

    #[cfg(test)]
    pub(crate) fn record_for_test(&self, source: InputSource, kind: InputEventKind) {
        self.inner.record(source, kind);
    }
}

#[cfg(windows)]
impl Drop for InputActivityMonitor {
    fn drop(&mut self) {
        self.inner.running.store(false, Ordering::Release);
        GLOBAL_MONITOR.store(std::ptr::null_mut(), Ordering::Release);
        let thread_id = self.inner.thread_id.load(Ordering::Acquire);
        if thread_id != 0 {
            unsafe {
                use windows_sys::Win32::UI::WindowsAndMessaging::{PostThreadMessageW, WM_QUIT};
                let _ = PostThreadMessageW(thread_id, WM_QUIT, 0, 0);
            }
        }
        if let Some(thread) = self
            .hook_thread
            .lock()
            .expect("input monitor hook thread poisoned")
            .take()
        {
            let _ = thread.join();
        }
    }
}

#[cfg(windows)]
fn classify(injected: bool, extra_info: usize, expected_marker: usize) -> InputSource {
    if !injected {
        InputSource::Hardware
    } else if extra_info == expected_marker {
        InputSource::AliceInjected
    } else {
        InputSource::OtherInjected
    }
}

#[cfg(windows)]
unsafe extern "system" fn low_level_input_proc(
    code: i32,
    wparam: windows_sys::Win32::Foundation::WPARAM,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) -> windows_sys::Win32::Foundation::LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::CallNextHookEx;

    if code >= 0 {
        let pointer = GLOBAL_MONITOR.load(Ordering::Acquire);
        if !pointer.is_null() {
            let monitor = &*pointer;
            if monitor.running.load(Ordering::Acquire) {
                record_hook_event(monitor, wparam as u32, lparam);
            }
        }
    }
    CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam)
}

#[cfg(windows)]
unsafe fn record_hook_event(
    monitor: &InputActivityMonitorInner,
    message: u32,
    lparam: windows_sys::Win32::Foundation::LPARAM,
) {
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        KBDLLHOOKSTRUCT, LLKHF_INJECTED, LLMHF_INJECTED, MSLLHOOKSTRUCT, WM_KEYDOWN, WM_KEYUP,
        WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE,
        WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_SYSKEYUP,
    };

    if matches!(message, WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP) {
        if lparam == 0 {
            monitor.record(InputSource::Unknown, InputEventKind::Unknown);
            return;
        }
        let event = &*(lparam as *const KBDLLHOOKSTRUCT);
        let source = classify(
            event.flags & LLKHF_INJECTED != 0,
            event.dwExtraInfo,
            monitor.expected_marker,
        );
        monitor.record(source, InputEventKind::Keyboard);
        return;
    }

    if matches!(
        message,
        WM_MOUSEMOVE
            | WM_LBUTTONDOWN
            | WM_LBUTTONUP
            | WM_RBUTTONDOWN
            | WM_RBUTTONUP
            | WM_MBUTTONDOWN
            | WM_MBUTTONUP
            | WM_MOUSEWHEEL
            | WM_MOUSEHWHEEL
    ) {
        if lparam == 0 {
            monitor.record(InputSource::Unknown, InputEventKind::Unknown);
            return;
        }
        let event = &*(lparam as *const MSLLHOOKSTRUCT);
        let source = classify(
            event.flags & LLMHF_INJECTED != 0,
            event.dwExtraInfo,
            monitor.expected_marker,
        );
        let kind = match message {
            WM_MOUSEMOVE => InputEventKind::MouseMove,
            WM_MOUSEWHEEL | WM_MOUSEHWHEEL => InputEventKind::MouseWheel,
            _ => InputEventKind::MouseButton,
        };
        monitor.record(source, kind);
    }
}

#[cfg(windows)]
fn hook_thread_main(
    inner: Arc<InputActivityMonitorInner>,
    ready_sender: std::sync::mpsc::Sender<bool>,
) {
    use windows_sys::Win32::System::Threading::GetCurrentThreadId;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, SetWindowsHookExW, TranslateMessage, UnhookWindowsHookEx,
        MSG, WH_KEYBOARD_LL, WH_MOUSE_LL,
    };

    inner
        .thread_id
        .store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    let keyboard_hook = unsafe {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(low_level_input_proc),
            std::ptr::null_mut(),
            0,
        )
    };
    let mouse_hook = unsafe {
        SetWindowsHookExW(
            WH_MOUSE_LL,
            Some(low_level_input_proc),
            std::ptr::null_mut(),
            0,
        )
    };
    if keyboard_hook.is_null() || mouse_hook.is_null() {
        if !keyboard_hook.is_null() {
            unsafe { UnhookWindowsHookEx(keyboard_hook) };
        }
        if !mouse_hook.is_null() {
            unsafe { UnhookWindowsHookEx(mouse_hook) };
        }
        inner.available.store(false, Ordering::Release);
        let _ = ready_sender.send(false);
        return;
    }

    inner.available.store(true, Ordering::Release);
    let _ = ready_sender.send(true);
    let mut message = unsafe { std::mem::zeroed::<MSG>() };
    while inner.running.load(Ordering::Acquire) {
        let result = unsafe { GetMessageW(&mut message, std::ptr::null_mut(), 0, 0) };
        if result <= 0 {
            break;
        }
        unsafe {
            TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    unsafe {
        UnhookWindowsHookEx(keyboard_hook);
        UnhookWindowsHookEx(mouse_hook);
    }
    inner.available.store(false, Ordering::Release);
}

#[cfg(all(test, windows))]
mod tests {
    use super::{classify, InputSource, ALICE_COMPUTER_INPUT_MARKER};

    #[test]
    fn fixed_marker_attributes_alice_input_without_treating_it_as_hardware() {
        assert_eq!(
            classify(
                true,
                ALICE_COMPUTER_INPUT_MARKER,
                ALICE_COMPUTER_INPUT_MARKER
            ),
            InputSource::AliceInjected
        );
        assert_eq!(
            classify(false, 0, ALICE_COMPUTER_INPUT_MARKER),
            InputSource::Hardware
        );
        assert_eq!(
            classify(true, 0x1234, ALICE_COMPUTER_INPUT_MARKER),
            InputSource::OtherInjected
        );
    }

    #[test]
    fn fixed_marker_from_another_host_is_not_alice_input() {
        let expected = 0xAABB_CCDD_EEFF_0011usize;
        assert_eq!(
            classify(true, ALICE_COMPUTER_INPUT_MARKER, expected),
            InputSource::OtherInjected
        );
    }
}
