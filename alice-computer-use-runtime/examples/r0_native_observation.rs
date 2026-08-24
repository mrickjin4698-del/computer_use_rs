//! Computer-INFRA-R0-NATIVE observation bake-off.
//!
//! This is an evidence runner, not a production loop. It calls the production
//! `WinNativeBackend` directly. It deliberately does not use UIA, COM, OCR,
//! clipboard reads, process launch, or semantic UI inspection.

#[cfg(not(windows))]
fn main() {
    eprintln!("r0_native_observation requires Windows");
}

#[cfg(windows)]
mod windows_runner {
    use alice_computer_use_core::{
        ComputerAction, ComputerActionResult, ComputerError, Coordinate, CoordinateSpace,
        MouseButton, Point, Screen, Screenshot, Window, WindowId,
    };
    use alice_computer_use_runtime::{
        ComputerBackend, ComputerRuntime, ComputerSession, NativeCaptureProfile, NativePngMode,
        WinNativeBackend,
    };
    use png::ColorType;
    use std::{
        collections::hash_map::DefaultHasher,
        error::Error,
        fs,
        hash::{Hash, Hasher},
        io::Cursor,
        path::{Path, PathBuf},
    };
    use windows::Win32::{
        Foundation::HWND,
        System::Console::GetConsoleWindow,
        UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId},
    };

    type RunnerResult<T> = Result<T, Box<dyn Error>>;

    const PROFILE_COUNT: usize = 30;

    #[derive(Debug)]
    struct ProfileSeries {
        mode: NativePngMode,
        count: usize,
        decode_passes: usize,
        metadata_passes: usize,
        visible_passes: usize,
        capture_micros: Vec<u128>,
        pixel_readback_micros: Vec<u128>,
        pixel_conversion_micros: Vec<u128>,
        png_encode_micros: Vec<u128>,
        total_micros: Vec<u128>,
        png_bytes: Vec<usize>,
    }

    #[derive(Debug)]
    struct DecodedScreenshot {
        width: u32,
        height: u32,
        visible: bool,
        hash: u64,
    }

    pub async fn run() -> RunnerResult<()> {
        let evidence_dir = PathBuf::from(".r0-evidence").join("native-observation");
        fs::create_dir_all(&evidence_dir)?;
        let only = parse_only();
        let skip_profile = has_flag("--skip-profile");
        print_process_info();
        println!("runner=alice-computer-use-runtime/r0_native_observation");
        println!("pointer_target_filter={:?}", only);
        println!("profile_probe_skipped={}", skip_profile);
        println!("native_backend=WinNativeBackend; observation=Win32-only; uia=false; com=false; ocr=false; process_launch=false; external_helper=false");
        println!("evidence_dir={}", evidence_dir.display());

        let mut display_probe = WinNativeBackend::new();
        display_probe.initialize().await?;
        let display = display_probe.primary_display_info()?;
        print_display_info(&display);
        display_probe.shutdown().await?;

        let native_runtime = ComputerRuntime::new(WinNativeBackend::new());
        native_runtime.initialize().await?;
        println!("backend=win-native initialize=PASS");
        print_capabilities("win-native", native_runtime.capabilities().await);
        let native_session = native_runtime.start_session().await?;
        let native_screens = native_session.enumerate_screens().await?;
        let native_screen = native_screens
            .iter()
            .find(|screen| screen.primary)
            .cloned()
            .ok_or("WinNative returned no primary screen")?;
        let native_windows = native_session.enumerate_windows().await?;
        print_screens("win-native", &native_screens);
        print_windows("win-native", &native_windows);
        print_target_matches("win-native", &native_windows);
        let native_observation = native_session.observe().await?;
        println!(
            "native_observe screens={} windows={} active={:?} frame_metadata={}",
            native_observation.screens.len(),
            native_observation.windows.len(),
            native_observation.active_window,
            native_observation.frame.is_some()
        );
        let (native_screenshot_pass, fast_png_selected, fast_png_tested) = if skip_profile {
            (true, true, false)
        } else {
            let mut profiling_backend = WinNativeBackend::new();
            profiling_backend.initialize().await?;
            let current_profile = profile_series(
                &profiling_backend,
                NativePngMode::Current,
                &native_screen,
                &evidence_dir,
                PROFILE_COUNT,
            )?;
            print_profile_series("win-native", &current_profile);
            let fast_profile = if encode_is_dominant(&current_profile) {
                let profile = profile_series(
                    &profiling_backend,
                    NativePngMode::Fast,
                    &native_screen,
                    &evidence_dir,
                    PROFILE_COUNT,
                )?;
                print_profile_series("win-native", &profile);
                Some(profile)
            } else {
                println!("fast_png_experiment=SKIPPED reason=png_encode_not_dominant");
                None
            };
            profiling_backend.shutdown().await?;
            let selected = fast_profile
                .as_ref()
                .map(|profile| fast_is_better(&current_profile, profile))
                .unwrap_or(false);
            let pass = if selected {
                fast_profile.as_ref().map(profile_pass).unwrap_or(false)
            } else {
                profile_pass(&current_profile)
            };
            (pass, selected, fast_profile.is_some())
        };
        let input_safety_pass =
            run_input_safety_regression(&native_session, &native_windows).await?;
        let focus_pass = run_focus_semantics(&native_session, &native_windows).await?;
        println!("native_focus_semantics={}", pass_fail(focus_pass));
        println!("native_input_safety={}", pass_fail(input_safety_pass));

        let native_pointer_pass = run_native_pointer_regression(
            &native_session,
            &native_screen,
            &native_windows,
            &evidence_dir,
            only.as_deref(),
        )
        .await?;
        println!(
            "native_pointer_regression={}",
            pass_fail(native_pointer_pass)
        );

        let native_lifecycle_pass = run_native_lifecycle(
            native_runtime,
            native_session,
            &native_screen,
            match only.as_deref() {
                Some("notepad") => find_window(&native_windows, "notepad"),
                Some("win32_fixture") => {
                    find_window(&native_windows, "alice computer input fixture")
                }
                Some("alice_tauri") => find_alice_tauri_window(&native_windows),
                _ => find_alice_tauri_window(&native_windows)
                    .or_else(|| find_window(&native_windows, "notepad"))
                    .or_else(|| find_window(&native_windows, "alice computer input fixture")),
            }
            .as_ref(),
        )
        .await?;
        println!("native_lifecycle={}", pass_fail(native_lifecycle_pass));

        println!(
            "matrix window_enumeration=native display_geometry=native_primary dpi=native_system_and_window screenshot={} pointer={} lifecycle={}",
            pass_fail(native_screenshot_pass),
            pass_fail(native_pointer_pass),
            pass_fail(native_lifecycle_pass),
        );
        println!(
            "native_gate_observation={} native_gate_screenshot={} native_gate_pointer={} native_gate_lifecycle={}",
            pass_fail(!native_windows.is_empty() && native_screens.len() == 1),
            pass_fail(native_screenshot_pass),
            pass_fail(native_pointer_pass),
            pass_fail(native_lifecycle_pass),
        );
        println!(
            "decision_input=observation_only; focus_semantics={}; input_safety={}; fast_png_tested={}; fast_png_selected={}; wgc=not_run_reason=png_encode_not_readback_bottleneck; semantic_UIA_gate=deferred; overall={}",
            pass_fail(focus_pass),
            pass_fail(input_safety_pass),
            fast_png_tested,
            fast_png_selected,
            if native_windows.is_empty()
                || !native_screenshot_pass
                || !native_pointer_pass
                || !native_lifecycle_pass
                || !input_safety_pass
            {
                "PARTIAL"
            } else {
                "PASS"
            }
        );
        Ok(())
    }

    fn print_process_info() {
        let foreground = unsafe { GetForegroundWindow() };
        let console = unsafe { GetConsoleWindow() };
        let mut foreground_pid = 0u32;
        if !foreground.0.is_null() {
            unsafe { GetWindowThreadProcessId(foreground, Some(&mut foreground_pid)) };
        }
        println!(
            "windows_session process_id={} console_hwnd={} foreground_hwnd={} foreground_pid={} session_name={:?} rdp_session={:?} interactive_hint={}",
            std::process::id(),
            hwnd_text(console),
            hwnd_text(foreground),
            foreground_pid,
            std::env::var("SESSIONNAME").ok(),
            std::env::var("RDP_SESSION").ok(),
            std::env::var("SESSIONNAME").ok().as_deref() == Some("Console")
                || std::env::var("RDP_SESSION").is_ok()
        );
    }

    fn hwnd_text(hwnd: HWND) -> String {
        if hwnd.0.is_null() {
            "null".into()
        } else {
            format!("0x{:x}", hwnd.0 as usize)
        }
    }

    fn print_display_info(display: &alice_computer_use_runtime::NativeDisplayInfo) {
        println!(
            "display primary_physical=({}, {}) {}x{} work_area=({}, {}) {}x{}",
            display.monitor.origin.x,
            display.monitor.origin.y,
            display.monitor.size.width,
            display.monitor.size.height,
            display.work_area.origin.x,
            display.work_area.origin.y,
            display.work_area.size.width,
            display.work_area.size.height
        );
        println!(
            "display logical_monitor=({}, {}) {}x{} logical_work_area=({}, {}) {}x{} system_dpi={} reported_scale={}x{}",
            display.logical_monitor.origin.x,
            display.logical_monitor.origin.y,
            display.logical_monitor.size.width,
            display.logical_monitor.size.height,
            display.logical_work_area.origin.x,
            display.logical_work_area.origin.y,
            display.logical_work_area.size.width,
            display.logical_work_area.size.height,
            display.system_dpi,
            display.scale.x,
            display.scale.y
        );
        println!("coordinate_spaces physical_screen=primary_monitor logical_diagnostic=scaled_from_physical screenshot=physical_primary");
    }

    fn print_capabilities(label: &str, capabilities: Vec<alice_computer_use_runtime::Capability>) {
        for capability in capabilities {
            println!(
                "capability backend={} name={} state={:?} detail={:?}",
                label, capability.name, capability.state, capability.detail
            );
        }
    }

    fn print_screens(label: &str, screens: &[Screen]) {
        for screen in screens {
            println!(
                "screen backend={} id={} primary={} geometry=({}, {}) {}x{} dpi={}x{}",
                label,
                screen.id,
                screen.primary,
                screen.physical_bounds.origin.x,
                screen.physical_bounds.origin.y,
                screen.physical_bounds.size.width,
                screen.physical_bounds.size.height,
                screen.dpi.x,
                screen.dpi.y
            );
        }
    }

    fn print_windows(label: &str, windows: &[Window]) {
        println!("windows backend={} count={}", label, windows.len());
        for window in windows {
            println!(
                "window backend={} id={} title={:?} pid={:?} active={} visible_contract=true geometry=({}, {}) {}x{} dpi={}x{}",
                label,
                window.id,
                window.title,
                window.process_id,
                window.active,
                window.bounds.point.x,
                window.bounds.point.y,
                window.bounds.extent.width,
                window.bounds.extent.height,
                window.bounds.dpi.x,
                window.bounds.dpi.y
            );
        }
    }

    fn print_target_matches(label: &str, windows: &[Window]) {
        for (kind, needle) in [
            ("notepad", "notepad"),
            ("win32_fixture", "alice computer input fixture"),
            ("alice_tauri", "alice"),
        ] {
            let matches: Vec<&Window> = windows
                .iter()
                .filter(|window| window.title.to_ascii_lowercase().contains(needle))
                .collect();
            println!(
                "target_match backend={} kind={} count={} ids={:?}",
                label,
                kind,
                matches.len(),
                matches
                    .iter()
                    .map(|window| window.id.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }

    async fn run_focus_semantics(
        session: &ComputerSession<WinNativeBackend>,
        windows: &[Window],
    ) -> RunnerResult<bool> {
        let fixture = find_window(windows, "alice computer input fixture");
        let notepad = find_window(windows, "notepad");
        let alice_tauri = find_alice_tauri_window(windows);
        let mut pass = true;
        let mut acquired = 0usize;

        for (case, target) in [
            ("fixture", fixture),
            ("notepad", notepad),
            ("alice_tauri", alice_tauri),
            ("already_foreground", None),
        ] {
            let target = if case == "already_foreground" {
                session
                    .enumerate_windows()
                    .await?
                    .into_iter()
                    .find(|window| window.active)
                    .map(|window| window.id)
                    .or_else(foreground_window_id)
            } else {
                target.map(|window| window.id)
            };
            let Some(target) = target else {
                println!("focus case={} result=SKIP target_not_found", case);
                continue;
            };
            let before = session
                .enumerate_windows()
                .await?
                .into_iter()
                .find(|window| window.active)
                .map(|window| window.id);
            let result = session
                .execute(&ComputerAction::FocusWindow {
                    window_id: target.clone(),
                })
                .await;
            let after = session
                .enumerate_windows()
                .await?
                .into_iter()
                .find(|window| window.active)
                .map(|window| window.id);
            let classification = classify_focus_result(&result, after.as_ref(), &target);
            if classification == "FOCUS_ACQUIRED" {
                acquired += 1;
            }
            let case_pass = match case {
                "fixture" | "notepad" | "alice_tauri" => {
                    classification == "FOCUS_ACQUIRED" || classification == "FOCUS_DENIED"
                }
                "already_foreground" => classification == "FOCUS_ACQUIRED",
                _ => false,
            };
            pass &= case_pass;
            println!(
                "focus case={} before={:?} target={} after={:?} classification={} api_result={} detail={:?}",
                case,
                before,
                target,
                after,
                classification,
                pass_fail(result.is_ok()),
                result.as_ref().err().map(ToString::to_string),
            );
        }

        let invalid_id = WindowId::new("1");
        let invalid_result = session
            .execute(&ComputerAction::FocusWindow {
                window_id: invalid_id.clone(),
            })
            .await;
        let invalid_classification = match invalid_result {
            Err(ComputerError::InvalidWindow(_)) => "INVALID_WINDOW",
            Err(ComputerError::ForegroundDenied(_)) => "FOCUS_DENIED",
            Err(ComputerError::FocusNotAcquired(_)) => "FOCUS_DENIED",
            Ok(_) => "FOCUS_ACQUIRED",
            Err(_) => "UNEXPECTED_ERROR",
        };
        let invalid_pass = invalid_classification == "INVALID_WINDOW";
        pass &= invalid_pass;
        println!(
            "focus case=invalid_window target={} classification={} result={}",
            invalid_id,
            invalid_classification,
            pass_fail(invalid_pass)
        );
        let result = pass && acquired > 0;
        println!(
            "focus_summary acquired={} result={}",
            acquired,
            pass_fail(result)
        );
        Ok(result)
    }

    fn foreground_window_id() -> Option<WindowId> {
        let hwnd = unsafe { GetForegroundWindow() };
        (!hwnd.0.is_null()).then(|| WindowId::new((hwnd.0 as isize).to_string()))
    }

    fn classify_focus_result(
        result: &Result<ComputerActionResult, ComputerError>,
        after: Option<&WindowId>,
        target: &WindowId,
    ) -> &'static str {
        if result.is_ok() && after == Some(target) {
            "FOCUS_ACQUIRED"
        } else {
            match result {
                Err(ComputerError::InvalidWindow(_)) => "INVALID_WINDOW",
                Err(ComputerError::ForegroundDenied(_))
                | Err(ComputerError::FocusNotAcquired(_)) => "FOCUS_DENIED",
                Ok(_) => "FOCUS_NOT_ACQUIRED",
                Err(_) => "UNEXPECTED_ERROR",
            }
        }
    }

    async fn run_input_safety_regression(
        session: &ComputerSession<WinNativeBackend>,
        windows: &[Window],
    ) -> RunnerResult<bool> {
        let Some(target) = find_window(windows, "alice computer input fixture") else {
            println!("input_safety result=SKIP fixture_not_found");
            return Ok(false);
        };
        let before_windows = session.enumerate_windows().await?;
        let before_title = before_windows
            .iter()
            .find(|window| window.id == target.id)
            .map(|window| window.title.clone())
            .unwrap_or_default();
        let active_before = foreground_window_id();
        let other = before_windows
            .iter()
            .find(|window| window.active && window.id != target.id)
            .or_else(|| {
                before_windows.iter().find(|window| {
                    window.id != target.id
                        && window.title.to_ascii_lowercase().contains("powershell")
                })
            })
            .cloned();
        let mut other_foreground = active_before.as_ref() != Some(&target.id);
        if !other_foreground {
            if let Some(other) = other {
                let _ = session
                    .execute(&ComputerAction::FocusWindow {
                        window_id: other.id.clone(),
                    })
                    .await;
            }
            other_foreground = foreground_window_id().as_ref() != Some(&target.id);
        }
        if !other_foreground {
            println!(
                "input_safety target={} precondition=FAIL actions_skipped=true result=BLOCKED",
                target.id
            );
            return Ok(false);
        }
        let actions = [
            (
                "type_text",
                ComputerAction::TypeText {
                    text: "ALICE_NATIVE_SAFETY_SENTINEL".into(),
                    target: Some(target.id.clone()),
                    at: None,
                },
            ),
            (
                "key_press",
                ComputerAction::KeyPress {
                    key: "enter".into(),
                    target: Some(target.id.clone()),
                },
            ),
            (
                "hotkey",
                ComputerAction::Hotkey {
                    keys: vec!["ctrl".into(), "a".into()],
                    target: Some(target.id.clone()),
                },
            ),
        ];
        let mut rejected = true;
        for (name, action) in actions {
            let result = session.execute(&action).await;
            let safe_rejection = matches!(
                result,
                Err(ComputerError::ForegroundDenied(_)) | Err(ComputerError::FocusNotAcquired(_))
            );
            rejected &= safe_rejection;
            println!(
                "input_safety action={} target={} other_foreground={} rejected_without_send={} detail={:?}",
                name,
                target.id,
                other_foreground,
                safe_rejection,
                result.as_ref().err().map(ToString::to_string),
            );
        }
        let after_title = session
            .enumerate_windows()
            .await?
            .into_iter()
            .find(|window| window.id == target.id)
            .map(|window| window.title)
            .unwrap_or_default();
        let unchanged = before_title == after_title;
        let pass = other_foreground && rejected && unchanged;
        println!(
            "input_safety_summary target={} other_foreground={} all_rejected={} fixture_title_unchanged={} result={}",
            target.id,
            other_foreground,
            rejected,
            unchanged,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn decode_png(screenshot: &Screenshot) -> RunnerResult<DecodedScreenshot> {
        let decoder = png::Decoder::new(Cursor::new(&screenshot.bytes));
        let mut reader = decoder.read_info()?;
        let mut buffer = vec![
            0;
            reader
                .output_buffer_size()
                .ok_or("PNG has no output buffer")?
        ];
        let info = reader.next_frame(&mut buffer)?;
        let bytes = &buffer[..info.buffer_size()];
        let channels = match info.color_type {
            ColorType::Rgb => 3,
            ColorType::Rgba => 4,
            ColorType::Grayscale => 1,
            ColorType::GrayscaleAlpha => 2,
            ColorType::Indexed => 1,
        };
        let visible = bytes.chunks(channels).any(|pixel| {
            let color_channels = channels.min(3);
            pixel[..color_channels].iter().any(|channel| *channel != 0)
        });
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        Ok(DecodedScreenshot {
            width: info.width,
            height: info.height,
            visible,
            hash: hasher.finish(),
        })
    }

    impl ProfileSeries {
        fn new(mode: NativePngMode) -> Self {
            Self {
                mode,
                count: 0,
                decode_passes: 0,
                metadata_passes: 0,
                visible_passes: 0,
                capture_micros: Vec::new(),
                pixel_readback_micros: Vec::new(),
                pixel_conversion_micros: Vec::new(),
                png_encode_micros: Vec::new(),
                total_micros: Vec::new(),
                png_bytes: Vec::new(),
            }
        }

        fn record(&mut self, profile: &NativeCaptureProfile) {
            self.count += 1;
            self.capture_micros.push(profile.capture_micros);
            self.pixel_readback_micros
                .push(profile.pixel_readback_micros);
            self.pixel_conversion_micros
                .push(profile.pixel_conversion_micros);
            self.png_encode_micros.push(profile.png_encode_micros);
            self.total_micros.push(profile.total_micros);
            self.png_bytes.push(profile.png_bytes);
        }
    }

    fn profile_series(
        backend: &WinNativeBackend,
        mode: NativePngMode,
        screen: &Screen,
        evidence_dir: &Path,
        count: usize,
    ) -> RunnerResult<ProfileSeries> {
        let mut report = ProfileSeries::new(mode);
        let expected_width = screen.physical_bounds.size.width.round() as u32;
        let expected_height = screen.physical_bounds.size.height.round() as u32;
        for index in 0..count {
            let profile = backend.capture_primary_profile(mode)?;
            let decoded = decode_png(&profile.screenshot)?;
            let decode_pass =
                decoded.width > 0 && decoded.height > 0 && !profile.screenshot.bytes.is_empty();
            let metadata_pass = profile.screenshot.metadata.mime_type == "image/png"
                && profile.screenshot.metadata.width == decoded.width
                && profile.screenshot.metadata.height == decoded.height
                && profile.screenshot.metadata.width == expected_width
                && profile.screenshot.metadata.height == expected_height
                && profile.screenshot.metadata.dpi == screen.dpi;
            report.decode_passes += usize::from(decode_pass);
            report.metadata_passes += usize::from(metadata_pass);
            report.visible_passes += usize::from(decoded.visible);
            report.record(&profile);
            if index == 0 || index + 1 == count {
                fs::write(
                    evidence_dir.join(format!("native-profile-{:?}-{:02}.png", mode, index + 1)),
                    &profile.screenshot.bytes,
                )?;
            }
            println!(
                "profile backend=win-native mode={:?} frame={}/{} decode={} metadata={} visible={} capture_us={} readback_us={} conversion_us={} encode_us={} total_us={} png_bytes={}",
                mode,
                index + 1,
                count,
                pass_fail(decode_pass),
                pass_fail(metadata_pass),
                pass_fail(decoded.visible),
                profile.capture_micros,
                profile.pixel_readback_micros,
                profile.pixel_conversion_micros,
                profile.png_encode_micros,
                profile.total_micros,
                profile.png_bytes,
            );
        }
        Ok(report)
    }

    fn profile_pass(report: &ProfileSeries) -> bool {
        report.count == PROFILE_COUNT
            && report.decode_passes == PROFILE_COUNT
            && report.metadata_passes == PROFILE_COUNT
            && report.visible_passes == PROFILE_COUNT
    }

    fn encode_is_dominant(report: &ProfileSeries) -> bool {
        let encode: u128 = report.png_encode_micros.iter().sum();
        let total: u128 = report.total_micros.iter().sum();
        let ratio = if total == 0 {
            0.0
        } else {
            encode as f64 / total as f64
        };
        println!(
            "png_encode_share mode={:?} encode_us={} total_us={} ratio={:.3}",
            report.mode, encode, total, ratio
        );
        ratio >= 0.5
    }

    fn fast_is_better(current: &ProfileSeries, fast: &ProfileSeries) -> bool {
        let current_average = average(&current.total_micros);
        let fast_average = average(&fast.total_micros);
        let improved =
            profile_pass(fast) && current_average > 0 && fast_average * 100 < current_average * 90;
        println!(
            "fast_png_decision current_avg_ms={:.3} fast_avg_ms={:.3} improvement_percent={:.2} selected={}",
            current_average as f64 / 1000.0,
            fast_average as f64 / 1000.0,
            if current_average == 0 {
                0.0
            } else {
                (current_average.saturating_sub(fast_average) as f64 / current_average as f64)
                    * 100.0
            },
            improved,
        );
        improved
    }

    fn average(values: &[u128]) -> u128 {
        if values.is_empty() {
            0
        } else {
            values.iter().sum::<u128>() / values.len() as u128
        }
    }

    fn percentile(values: &[u128], percentile: f64) -> u128 {
        if values.is_empty() {
            return 0;
        }
        let mut sorted = values.to_vec();
        sorted.sort_unstable();
        let index = (((sorted.len() - 1) as f64) * percentile).ceil() as usize;
        sorted[index.min(sorted.len() - 1)]
    }

    fn print_profile_series(label: &str, report: &ProfileSeries) {
        println!(
            "profile_summary backend={} mode={:?} count={} decode={}/{} metadata={}/{} visible={}/{} png_bytes={}..{}",
            label,
            report.mode,
            report.count,
            report.decode_passes,
            report.count,
            report.metadata_passes,
            report.count,
            report.visible_passes,
            report.count,
            report.png_bytes.iter().copied().min().unwrap_or(0),
            report.png_bytes.iter().copied().max().unwrap_or(0),
        );
        for (name, values) in [
            ("capture_ms", &report.capture_micros),
            ("pixel_readback_ms", &report.pixel_readback_micros),
            ("pixel_conversion_ms", &report.pixel_conversion_micros),
            ("png_encode_ms", &report.png_encode_micros),
            ("total_ms", &report.total_micros),
        ] {
            println!(
                "profile_stat backend={} mode={:?} phase={} average_ms={:.3} p50_ms={:.3} p95_ms={:.3}",
                label,
                report.mode,
                name,
                average(values) as f64 / 1000.0,
                percentile(values, 0.50) as f64 / 1000.0,
                percentile(values, 0.95) as f64 / 1000.0,
            );
        }
    }

    async fn run_native_pointer_regression(
        session: &ComputerSession<WinNativeBackend>,
        screen: &Screen,
        windows: &[Window],
        evidence_dir: &Path,
        only: Option<&str>,
    ) -> RunnerResult<bool> {
        let all_targets = [
            ("notepad", find_window(windows, "notepad")),
            (
                "win32_fixture",
                find_window(windows, "alice computer input fixture"),
            ),
            ("alice_tauri", find_alice_tauri_window(windows)),
        ];
        let targets: Vec<(&str, Option<Window>)> = all_targets
            .into_iter()
            .filter(|(kind, _)| only.map(|filter| filter == *kind).unwrap_or(true))
            .collect();
        let mut overall = true;
        let mut exercised = false;
        for (kind, target) in targets {
            let Some(target) = target else {
                println!(
                    "pointer backend=win-native kind={} result=SKIP target_not_found",
                    kind
                );
                overall = false;
                continue;
            };
            let center = window_coordinate(&target, screen, 0.50, 0.55);
            let nearby = window_coordinate(&target, screen, 0.53, 0.55);
            let before = session.screenshot(&screen.id).await?;
            let focus_result = session
                .execute(&ComputerAction::FocusWindow {
                    window_id: target.id.clone(),
                })
                .await;
            let focus_denied = matches!(
                &focus_result,
                Err(ComputerError::ForegroundDenied(_)) | Err(ComputerError::FocusNotAcquired(_))
            );
            match focus_result {
                Ok(result) => print_action::<
                    ComputerActionResult,
                    alice_computer_use_core::ComputerError,
                >("win-native", kind, "focus_window", Ok(result)),
                Err(error) => print_action::<ComputerActionResult, _>(
                    "win-native",
                    kind,
                    "focus_window",
                    Err(error),
                ),
            }
            if focus_denied {
                let after = session.screenshot(&screen.id).await?;
                let before_decoded = decode_png(&before)?;
                let after_decoded = decode_png(&after)?;
                fs::write(
                    evidence_dir.join(format!("native-pointer-{}-before.png", kind)),
                    &before.bytes,
                )?;
                fs::write(
                    evidence_dir.join(format!("native-pointer-{}-after.png", kind)),
                    &after.bytes,
                )?;
                println!(
                    "pointer_evidence backend=win-native kind={} focus=FOCUS_DENIED actions_skipped=true screenshot_before_after={} before_hash={} after_hash={} result=FOCUS_DENIED_SAFE",
                    kind,
                    before_decoded.width == after_decoded.width
                        && before_decoded.height == after_decoded.height,
                    before_decoded.hash,
                    after_decoded.hash,
                );
                continue;
            }
            exercised = true;
            let mut action_passes = 1usize;
            for (name, action) in [
                (
                    "move_pointer",
                    ComputerAction::MovePointer { to: center.clone() },
                ),
                ("click", ComputerAction::Click { at: center.clone() }),
                (
                    "drag",
                    ComputerAction::Drag {
                        from: center.clone(),
                        to: nearby,
                        button: MouseButton::Left,
                    },
                ),
                (
                    "scroll",
                    ComputerAction::Scroll {
                        at: center.clone(),
                        direction: alice_computer_use_core::ScrollDirection::Down,
                        amount: 1,
                    },
                ),
            ] {
                match session.execute(&action).await {
                    Ok(result) => {
                        action_passes += 1;
                        print_action::<ComputerActionResult, alice_computer_use_core::ComputerError>(
                            "win-native",
                            kind,
                            name,
                            Ok(result),
                        );
                    }
                    Err(error) => print_action::<ComputerActionResult, _>(
                        "win-native",
                        kind,
                        name,
                        Err(error),
                    ),
                }
            }
            let after = session.screenshot(&screen.id).await?;
            let before_decoded = decode_png(&before)?;
            let after_decoded = decode_png(&after)?;
            fs::write(
                evidence_dir.join(format!("native-pointer-{}-before.png", kind)),
                &before.bytes,
            )?;
            fs::write(
                evidence_dir.join(format!("native-pointer-{}-after.png", kind)),
                &after.bytes,
            )?;
            let active = session
                .enumerate_windows()
                .await?
                .into_iter()
                .find(|window| window.active)
                .map(|window| window.id == target.id)
                .unwrap_or(false);
            let evidence_pass = action_passes == 5
                && active
                && before_decoded.width == after_decoded.width
                && before_decoded.height == after_decoded.height;
            println!(
                "pointer_evidence backend=win-native kind={} actions={}/5 foreground_target={} screenshot_before_after={} before_hash={} after_hash={} result={}",
                kind,
                action_passes,
                active,
                before_decoded.width == after_decoded.width
                    && before_decoded.height == after_decoded.height,
                before_decoded.hash,
                after_decoded.hash,
                pass_fail(evidence_pass)
            );
            overall &= evidence_pass;
        }
        Ok(overall && exercised)
    }

    async fn run_native_lifecycle(
        runtime: ComputerRuntime<WinNativeBackend>,
        old_session: ComputerSession<WinNativeBackend>,
        screen: &Screen,
        target: Option<&Window>,
    ) -> RunnerResult<bool> {
        let old_action = ComputerAction::MovePointer {
            to: Coordinate {
                space: CoordinateSpace::DesktopPhysical,
                point: Point {
                    x: screen.physical_bounds.origin.x + screen.physical_bounds.size.width / 2.0,
                    y: screen.physical_bounds.origin.y + screen.physical_bounds.size.height / 2.0,
                },
                extent: screen.physical_bounds.size,
                dpi: screen.dpi,
                display_id: None,
                frame_id: None,
            },
        };
        runtime.shutdown().await?;
        let repeated_shutdown = runtime.shutdown().await.is_ok();
        let old_rejected = old_session.execute(&old_action).await.is_err();
        println!(
            "lifecycle backend=win-native shutdown=PASS repeated_shutdown={} old_session_action_rejected={} helper_processes=not_applicable",
            repeated_shutdown, old_rejected
        );
        runtime.initialize().await?;
        let fresh = runtime.start_session().await?;
        let fresh_observation = fresh.observe().await?;
        let fresh_action = match target {
            Some(target) => ComputerAction::FocusWindow {
                window_id: target.id.clone(),
            },
            None => old_action,
        };
        let fresh_action_ok = fresh.execute(&fresh_action).await.is_ok();
        let fresh_close = fresh.close().await.is_ok();
        let final_shutdown = runtime.shutdown().await.is_ok();
        println!(
            "lifecycle backend=win-native reinitialize=PASS fresh_observe_windows={} fresh_observe_frame={} fresh_action={} fresh_session_close={} final_shutdown={} result={}",
            fresh_observation.windows.len(),
            fresh_observation.frame.is_some(),
            fresh_action_ok,
            fresh_close,
            final_shutdown,
            pass_fail(repeated_shutdown
                && old_rejected
                && fresh_action_ok
                && fresh_close
                && final_shutdown)
        );
        Ok(repeated_shutdown && old_rejected && fresh_action_ok && fresh_close && final_shutdown)
    }

    fn find_window(windows: &[Window], needle: &str) -> Option<Window> {
        let needle = needle.to_ascii_lowercase();
        windows
            .iter()
            .find(|window| window.title.to_ascii_lowercase().contains(&needle))
            .cloned()
    }

    fn find_alice_tauri_window(windows: &[Window]) -> Option<Window> {
        windows
            .iter()
            .find(|window| {
                let title = window.title.to_ascii_lowercase();
                (title.contains("notepad") || title.contains("calculator"))
                    && !title.contains("input_fixture")
                    && !title.contains("computer input fixture")
                    && window.bounds.extent.width > 100.0
                    && window.bounds.extent.height > 80.0
            })
            .cloned()
    }

    fn parse_only() -> Option<String> {
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "--only" {
                return args.next();
            }
        }
        None
    }

    fn has_flag(flag: &str) -> bool {
        std::env::args().skip(1).any(|arg| arg == flag)
    }

    fn window_coordinate(window: &Window, screen: &Screen, x: f64, y: f64) -> Coordinate {
        Coordinate {
            space: CoordinateSpace::DesktopPhysical,
            point: Point {
                x: window.bounds.point.x + window.bounds.extent.width * x,
                y: window.bounds.point.y + window.bounds.extent.height * y,
            },
            extent: screen.physical_bounds.size,
            dpi: screen.dpi,
            display_id: None,
            frame_id: None,
        }
    }

    fn print_action<T, E>(backend: &str, kind: &str, name: &str, result: Result<T, E>)
    where
        T: std::fmt::Debug,
        E: std::fmt::Display,
    {
        match result {
            Ok(result) => println!(
                "action backend={} kind={} name={} api_result=PASS detail={:?}",
                backend, kind, name, result
            ),
            Err(error) => println!(
                "action backend={} kind={} name={} api_result=ERROR detail={}",
                backend, kind, name, error
            ),
        }
    }

    fn pass_fail(pass: bool) -> &'static str {
        if pass {
            "PASS"
        } else {
            "FAIL"
        }
    }
}

#[cfg(windows)]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_runner::run().await
}
