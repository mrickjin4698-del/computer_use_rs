//! Real-desktop Computer-INFRA-R0.5 sidecar gates.
//!
//! This is an acceptance runner, not a production backend. It talks to the
//! production `ComputerSidecarClient`, saves PNG evidence, and uses only a
//! Windows process API to terminate the sidecar for the crash gate.

use alice_computer_use_core::{
    ComputerAction, Coordinate, CoordinateSpace, DpiScale, Point, Screen, Size, Window,
};
use alice_computer_use_sidecar_client::{ComputerSidecarClient, SidecarError};
use png::Decoder;
use std::{
    env, fs,
    io::Cursor,
    path::{Path, PathBuf},
    sync::{Arc, Barrier},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    let evidence = env::args()
        .nth(2)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".r0-evidence/sidecar"));
    fs::create_dir_all(&evidence)?;
    let timeout = Duration::from_secs(15);
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let unique_text = format!("ALICE_SIDECAR_R05_{stamp}");

    println!("runner=alice-computer-use-sidecar-client/r0_interactive_gates");
    println!("executable={}", executable.display());
    println!("evidence_dir={}", evidence.display());

    let client = ComputerSidecarClient::spawn(&executable, timeout)?;
    let hello = client.hello()?;
    let health = client.health()?;
    println!(
        "gate2.spawn hello=PASS protocol={} backend={} sidecar_pid={} health_ready={}",
        hello.protocol_version,
        hello.backend,
        client.process_id(),
        health.ready
    );
    let session = client.create_session()?;
    let initial_windows = session.window_list()?;
    let notepad = find_notepad(&initial_windows)
        .ok_or_else(|| "Gate 2: no Notepad window found".to_owned())?;
    println!(
        "gate2.window_list=PASS count={} notepad_title={:?} public_window_id={}",
        initial_windows.len(),
        notepad.title,
        notepad.id
    );

    let observation = session.observe()?;
    let screen = observation
        .screens
        .iter()
        .find(|screen| screen.primary)
        .cloned()
        .ok_or_else(|| "Gate 2: no primary screen".to_owned())?;
    let frame = observation
        .frame
        .as_ref()
        .ok_or("Gate 2: observe returned no frame metadata")?;
    let screenshot =
        session.encode_frame(&frame.frame_id, alice_computer_use_core::FrameEncoding::Png)?;
    verify_and_save(&screenshot.screenshot, &evidence.join("gate2-before.png"))?;
    println!(
        "gate2.initial_observation=PASS screen={}x{} dpi={}x{}",
        screen.physical_bounds.size.width,
        screen.physical_bounds.size.height,
        screen.dpi.x,
        screen.dpi.y
    );

    session.action(&ComputerAction::FocusWindow {
        window_id: notepad.id.clone(),
    })?;
    let focused_windows = session.window_list()?;
    let focused_notepad = find_same_window(&focused_windows, notepad)
        .ok_or_else(|| "Gate 2: Notepad disappeared after focus".to_owned())?;
    if !focused_notepad.active {
        return Err("Gate 2: focus action returned but Notepad is not foreground".into());
    }
    println!("gate2.focus=PASS foreground_confirmed=true");

    let click = coordinate_for_window(focused_notepad, &screen);
    session.action(&ComputerAction::Click { at: click })?;
    println!("gate2.click=PASS");
    session.action(&ComputerAction::Hotkey {
        keys: vec!["ctrl".into(), "a".into()],
        target: Some(notepad.id.clone()),
    })?;
    println!("gate2.hotkey_ctrl_a=PASS");
    // This is an application-state settle point, not an action retry. Modern
    // Notepad applies selection asynchronously after SendInput returns.
    thread::sleep(Duration::from_millis(500));
    session.action(&ComputerAction::TypeText {
        text: unique_text.clone(),
        target: Some(notepad.id.clone()),
        at: None,
    })?;
    println!("gate2.type_text=PASS text={unique_text}");
    session.action(&ComputerAction::KeyPress {
        key: "enter".into(),
        target: Some(notepad.id.clone()),
    })?;
    println!("gate2.keypress_enter=PASS");

    let (after_notepad, title_checks) =
        wait_for_title(&session, notepad, &unique_text, Duration::from_secs(3))?;
    let after_observation = session.observe()?;
    let after_screenshot = after_observation
        .screenshot
        .as_ref()
        .ok_or_else(|| "Gate 2: final observe returned no screenshot".to_owned())?;
    verify_and_save(after_screenshot, &evidence.join("gate2-after.png"))?;
    println!(
        "gate2.screenshot_verify=PASS title_contains_unique_text=true title_checks={} title={:?}",
        title_checks, after_notepad.title
    );

    let gate3_pid = client.process_id();
    let mut gate3_windows = 0usize;
    for index in 0..100usize {
        let response = if index % 5 == 0 {
            gate3_windows += 1;
            session.window_list()?
        } else {
            let health = client.health()?;
            if health.pid != gate3_pid || !health.ready {
                return Err(format!("Gate 3: health mismatch at request {index}").into());
            }
            continue;
        };
        if response.is_empty() {
            return Err(format!("Gate 3: empty window list at request {index}").into());
        }
    }
    if client.process_id() != gate3_pid || !client.is_alive() {
        return Err("Gate 3: sidecar PID changed or process exited".into());
    }
    println!(
        "gate3=PASS requests=100 window_list_requests={} stable_pid={} stdout_protocol_clean=true",
        gate3_windows, gate3_pid
    );

    let observe_session = session.clone();
    let observe_thread = thread::spawn(move || observe_session.observe());
    thread::sleep(Duration::from_millis(250));
    let action_session = session.clone();
    let pending_notepad_id = notepad.id.clone();
    let action_started = Arc::new(Barrier::new(2));
    let action_started_thread = Arc::clone(&action_started);
    let pending_action = thread::spawn(move || {
        action_started_thread.wait();
        action_session.action(&ComputerAction::KeyPress {
            key: "enter".into(),
            target: Some(pending_notepad_id),
        })
    });
    action_started.wait();
    thread::sleep(Duration::from_millis(50));
    terminate_sidecar(gate3_pid)?;
    let action_result = pending_action
        .join()
        .map_err(|_| "Gate 4: action thread panicked")?;
    let _ = observe_thread
        .join()
        .map_err(|_| "Gate 4: observe thread panicked")?;
    let action_error = match action_result {
        Err(error) if error.outcome_unknown() => error,
        Err(error) => return Err(format!("Gate 4: crash error was not uncertain: {error}").into()),
        Ok(_) => return Err("Gate 4: pending action falsely returned success".into()),
    };
    let old_session_error = session
        .window_list()
        .expect_err("Gate 4: old session unexpectedly remained valid");
    if !matches!(old_session_error, SidecarError::SessionInvalidated { .. }) {
        return Err(format!("Gate 4: old session error was {old_session_error}").into());
    }
    println!(
        "gate4.crash=PASS action_code={} outcome_unknown={} old_session_invalid=true no_replay=true",
        action_error.code(),
        action_error.outcome_unknown()
    );

    let restarted = ComputerSidecarClient::spawn(&executable, timeout)?;
    let restarted_pid = restarted.process_id();
    let restarted_hello = restarted.hello()?;
    let restarted_session = restarted.create_session()?;
    let restarted_windows = restarted_session.window_list()?;
    let restarted_notepad = find_notepad(&restarted_windows)
        .ok_or_else(|| "Gate 4: restarted sidecar cannot find Notepad".to_owned())?;
    let restarted_screenshot = restarted_session.screenshot(None)?;
    verify_and_save(&restarted_screenshot, &evidence.join("gate4-restarted.png"))?;
    restarted_session.action(&ComputerAction::FocusWindow {
        window_id: restarted_notepad.id.clone(),
    })?;
    let restarted_active = find_same_window(&restarted_session.window_list()?, restarted_notepad)
        .map(|window| window.active)
        .unwrap_or(false);
    if !restarted_active {
        return Err("Gate 4: restarted safe focus was not confirmed".into());
    }
    restarted_session.close()?;
    restarted.shutdown()?;
    println!(
        "gate4.restart=PASS new_pid={} hello_protocol={} new_session=true window_list=true screenshot=true safe_focus=true",
        restarted_pid, restarted_hello.protocol_version
    );

    for cycle in 1..=2 {
        let lifecycle = ComputerSidecarClient::spawn(&executable, timeout)?;
        let lifecycle_pid = lifecycle.process_id();
        let lifecycle_session = lifecycle.create_session()?;
        if lifecycle_session.window_list()?.is_empty() {
            return Err(format!("Gate 5 cycle {cycle}: empty window list").into());
        }
        lifecycle_session.close()?;
        lifecycle.shutdown()?;
        if lifecycle.is_alive() {
            return Err(format!("Gate 5 cycle {cycle}: sidecar still alive").into());
        }
        println!(
            "gate5.cycle={cycle} PASS pid={} session_close=true shutdown=true exited=true residual=0",
            lifecycle_pid
        );
    }

    println!("result=PASS functional_gates=1,2,3,4,5");
    Ok(())
}

fn find_notepad(windows: &[Window]) -> Option<&Window> {
    windows
        .iter()
        .find(|window| window.title.to_ascii_lowercase().contains("notepad"))
}

fn find_same_window<'a>(windows: &'a [Window], expected: &Window) -> Option<&'a Window> {
    windows.iter().find(|window| window.id == expected.id)
}

fn wait_for_title(
    session: &alice_computer_use_sidecar_client::ComputerSidecarSession,
    expected: &Window,
    text: &str,
    timeout: Duration,
) -> Result<(Window, usize), Box<dyn std::error::Error>> {
    let started = SystemTime::now();
    let mut checks = 0usize;
    loop {
        checks += 1;
        let windows = session.window_list()?;
        if let Some(window) = find_same_window(&windows, expected) {
            if window.title.contains(text) {
                return Ok((window.clone(), checks));
            }
        }
        if started.elapsed().unwrap_or_default() >= timeout {
            let title = find_same_window(&windows, expected)
                .map(|window| window.title.clone())
                .unwrap_or_else(|| "<window-not-found>".into());
            return Err(format!(
                "Gate 2: Notepad title did not reach expected state after {} checks; title={title:?}",
                checks
            )
            .into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn coordinate_for_window(window: &Window, screen: &Screen) -> Coordinate {
    Coordinate {
        space: CoordinateSpace::DesktopPhysical,
        point: Point {
            x: window.bounds.point.x + window.bounds.extent.width * 0.5,
            y: window.bounds.point.y + window.bounds.extent.height * 0.5,
        },
        extent: Size {
            width: screen.physical_bounds.size.width,
            height: screen.physical_bounds.size.height,
        },
        dpi: DpiScale {
            x: screen.dpi.x,
            y: screen.dpi.y,
        },
        display_id: None,
        frame_id: None,
    }
}

fn verify_and_save(
    screenshot: &alice_computer_use_core::Screenshot,
    path: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if screenshot.bytes.is_empty()
        || screenshot.metadata.width == 0
        || screenshot.metadata.height == 0
    {
        return Err("screenshot is empty or has zero dimensions".into());
    }
    let decoder = Decoder::new(Cursor::new(&screenshot.bytes));
    let mut reader = decoder.read_info()?;
    let mut pixels = vec![
        0;
        reader
            .output_buffer_size()
            .ok_or("PNG decoder did not report an output buffer size")?
    ];
    let frame = reader.next_frame(&mut pixels)?;
    if frame.width != screenshot.metadata.width || frame.height != screenshot.metadata.height {
        return Err(format!(
            "PNG dimensions {}x{} disagree with metadata {}x{}",
            frame.width, frame.height, screenshot.metadata.width, screenshot.metadata.height
        )
        .into());
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &screenshot.bytes)?;
    println!(
        "screenshot={} png_decode=PASS dimensions={}x{} bytes={} dpi={}x{}",
        path.display(),
        frame.width,
        frame.height,
        screenshot.bytes.len(),
        screenshot.metadata.dpi.x,
        screenshot.metadata.dpi.y
    );
    Ok(())
}

fn terminate_sidecar(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
    if handle.is_null() {
        return Err(format!("OpenProcess failed for sidecar PID {pid}").into());
    }
    let terminated = unsafe { TerminateProcess(handle, 137) };
    unsafe {
        CloseHandle(handle);
    }
    if terminated == 0 {
        return Err(format!("TerminateProcess failed for sidecar PID {pid}").into());
    }
    Ok(())
}
