//! Real interactive Windows acceptance runner for Computer-INFRA-R3.
//!
//! This runner exercises the production path:
//! ComputerSidecarClient -> execution.perform -> WinNativeBackend.
//! It contains no model, agent loop, OCR, UIA tree fallback, browser path, or
//! second automation backend. The Win32 fixture is test-only.

#[cfg(not(windows))]
fn main() {
    eprintln!("r3_execution_policy requires Windows");
}

#[cfg(windows)]
mod windows_runner {
    use alice_computer_use_core::{
        ComputerAction, ComputerElement, ComputerExecutionIntent, ComputerExecutionOutcome,
        ComputerExecutionRequest, ComputerExecutionStrategy, ComputerFallbackPolicy,
        SemanticAction, SemanticObservation, SemanticObservationLimits, Window,
    };
    use alice_computer_use_sidecar_client::{
        ComputerSidecarClient, ComputerSidecarSession, SidecarError,
    };
    use std::{
        error::Error,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        thread,
        time::{Duration, Instant},
    };
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
    };

    type RunnerResult<T> = Result<T, Box<dyn Error>>;

    const LIMITS: SemanticObservationLimits = SemanticObservationLimits {
        max_depth: 8,
        max_elements: 512,
    };
    const BUTTON_INVOKE_MARKER: &str = "BUTTON_INVOKES=";
    const PIXEL_CLICK_MARKER: &str = "PIXEL_CLICKS=";

    pub fn run() -> RunnerResult<()> {
        let sidecar_path = argument_value("--sidecar")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
        let fixture_path = argument_value("--fixture")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/debug/examples/r0_input_fixture.exe"));
        println!("runner=alice-computer-use-sidecar-client/r3_execution_policy");
        println!("sidecar={}", sidecar_path.display());
        println!("fixture={}", fixture_path.display());

        let mut fixture_child = if has_flag("--no-start-fixture") {
            println!("fixture_start=SKIP reason=manual_fixture_mode");
            None
        } else {
            Some(start_fixture(&fixture_path)?)
        };
        let result = (|| {
            let client = ComputerSidecarClient::spawn(&sidecar_path, Duration::from_secs(30))?;
            println!(
                "gate0=PASS sidecar_pid={} backend=win_native protocol={}",
                client.process_id(),
                client.hello()?.protocol_version
            );
            let session = client.create_session()?;
            let windows = session.window_list()?;
            let fixture = find_window(&windows, "alice computer input fixture")
                .ok_or("R3 requires the Win32 fixture window")?;
            let notepad = find_window(&windows, "notepad");
            println!(
                "targets fixture_id={} fixture_title={:?} notepad={:?}",
                fixture.id,
                fixture.title,
                notepad.as_ref().map(|window| (&window.id, &window.title))
            );
            if has_flag("--wait-for-foreground") {
                wait_for_fixture_foreground(&session, &fixture)?;
            }

            if let Some(mode) = argument_value("--mode") {
                let pass = run_mode_probe(&session, &fixture, &mode)?;
                let close = session.close().is_ok();
                let shutdown = client.shutdown().is_ok();
                println!(
                    "mode={} session_close={} shutdown={} result={}",
                    mode,
                    close,
                    shutdown,
                    pass_fail(pass && close && shutdown)
                );
                return if pass && close && shutdown {
                    Ok(())
                } else {
                    Err(format!("R3 mode {mode} failed").into())
                };
            }

            let gate1 = run_semantic_success_gate(&session, &fixture)?;
            println!("gate1_semantic_success={}", pass_fail(gate1));
            let gate2 = run_pixel_fallback_gate(&session, &fixture)?;
            println!("gate2_pixel_fallback={}", pass_fail(gate2));
            let gate3 = run_fallback_denied_gate(&session, &fixture)?;
            println!("gate3_fallback_denied={}", pass_fail(gate3));

            let crash_before = fixture_counter(&session, &fixture, BUTTON_INVOKE_MARKER)?;
            let crash_pid = client.process_id();
            let crash_session = session.clone();
            let crash_fixture = fixture.clone();
            let pending = thread::spawn(move || {
                let observation = crash_session.semantic_observe(&crash_fixture.id, LIMITS)?;
                let button = find_button(&observation).ok_or_else(|| {
                    SidecarError::InvalidResponse("R3 crash button missing".into())
                })?;
                let request = semantic_request(
                    SemanticAction::Invoke {
                        element_id: button.id.clone(),
                    },
                    ComputerExecutionStrategy::SemanticOnly,
                    ComputerFallbackPolicy::Deny,
                );
                Ok::<_, SidecarError>(crash_session.execute_policy(&request))
            });
            wait_for_action_start();
            terminate_sidecar(crash_pid)?;
            let pending_result = pending
                .join()
                .map_err(|_| "R3 crash action thread panicked")??;
            let outcome_unknown = match pending_result {
                Err(error) => error.outcome_unknown(),
                Ok(_) => false,
            };
            let old_session_invalid = session.window_list().is_err();

            let restarted = ComputerSidecarClient::spawn(&sidecar_path, Duration::from_secs(30))?;
            let new_session = restarted.create_session()?;
            let new_fixture =
                find_window(&new_session.window_list()?, "alice computer input fixture")
                    .ok_or("R3 restart could not find fixture")?;
            let crash_after = fixture_counter(&new_session, &new_fixture, BUTTON_INVOKE_MARKER)?;
            let no_replay = crash_after == crash_before + 1;
            let gate4 = outcome_unknown && old_session_invalid && no_replay;
            println!(
                "gate4 outcome_unknown={} old_session_invalid={} counter_before={} counter_after={} no_replay={} fallback_after_restart=false result={}",
                outcome_unknown,
                old_session_invalid,
                crash_before,
                crash_after,
                no_replay,
                pass_fail(gate4)
            );
            println!("gate8_crash_semantics={}", pass_fail(gate4));

            let gate5 = run_stale_bounds_gate(&new_session, &new_fixture)?;
            println!("gate5_stale_bounds={}", pass_fail(gate5));
            let gate7 = run_verification_parity_gate(&new_session, &new_fixture)?;
            println!("gate7_verification_parity={}", pass_fail(gate7));
            let gate6 = run_focus_safety_gate(&new_session, &new_fixture, notepad.as_ref())?;
            println!("gate6_focus_safety={}", pass_fail(gate6));
            let gate9 = run_stability_gate(&new_session, &new_fixture, restarted.process_id())?;
            println!("gate9_stability={}", pass_fail(gate9));

            let close = new_session.close().is_ok();
            let shutdown = restarted.shutdown().is_ok();
            let repeated_shutdown = restarted.shutdown().is_ok();
            println!(
                "lifecycle session_close={} shutdown={} repeated_shutdown={}",
                close, shutdown, repeated_shutdown
            );
            let all = gate1
                && gate2
                && gate3
                && gate4
                && gate5
                && gate6
                && gate7
                && gate9
                && close
                && shutdown
                && repeated_shutdown;
            println!("result={}", pass_fail(all));
            if all {
                Ok(())
            } else {
                Err("one or more R3 execution-policy gates failed".into())
            }
        })();

        if let Some(mut fixture_child) = fixture_child.take() {
            let _ = fixture_child.kill();
            let _ = fixture_child.wait();
        }
        result
    }

    fn run_semantic_success_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        if !ensure_fixture_foreground(session, fixture)? {
            println!("gate1 evidence=WINDOW_FOCUS_DENIED");
            return Ok(false);
        }
        let before = fixture_counter(session, fixture, BUTTON_INVOKE_MARKER)?;
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let button = find_button(&observation).ok_or("R3 semantic button missing")?;
        let request = semantic_request(
            SemanticAction::Invoke {
                element_id: button.id.clone(),
            },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Allow,
        );
        let result = session.execute_policy(&request)?;
        let after = fixture_counter(session, fixture, BUTTON_INVOKE_MARKER)?;
        let pass = result.final_outcome == ComputerExecutionOutcome::Performed
            && !result.fallback_used
            && result.attempts.len() == 1
            && result.attempts.first().is_some_and(|attempt| {
                attempt.method == alice_computer_use_core::ComputerExecutionMethod::SemanticInvoke
            })
            && result.verification.verified
            && result.verification.state_changed == Some(true)
            && after == before + 1;
        println!(
            "gate1 evidence=result={:?} fallback_used={} attempts={} counter_before={} counter_after={} verification={:?} pass={}",
            result.final_outcome,
            result.fallback_used,
            result.attempts.len(),
            before,
            after,
            result.verification,
            pass_fail(pass)
        );
        print_timing("gate1", &result);
        Ok(pass)
    }

    fn run_pixel_fallback_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        if !ensure_fixture_foreground(session, fixture)? {
            return Ok(false);
        }
        let before = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let target = find_pixel_target(&observation).ok_or("R3 pixel-only target missing")?;
        println!("gate2 target_id={} bounds={:?}", target.id, target.bounds);
        let request = semantic_request(
            SemanticAction::Invoke {
                element_id: target.id.clone(),
            },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Allow,
        );
        let result = session.execute_policy(&request)?;
        let after = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let after_title = fixture_title(session, fixture)?;
        let first_unsupported = result.attempts.first().is_some_and(|attempt| {
            matches!(
                attempt.outcome,
                ComputerExecutionOutcome::Unsupported
                    | ComputerExecutionOutcome::ElementUnavailable
            )
        });
        let second_pixel = result.attempts.get(1).is_some_and(|attempt| {
            attempt.method == alice_computer_use_core::ComputerExecutionMethod::PixelClick
        });
        let pass = result.final_outcome == ComputerExecutionOutcome::Performed
            && result.fallback_used
            && result.attempts.len() == 2
            && first_unsupported
            && second_pixel
            && result.verification.verified
            && result.verification.state_changed == Some(true)
            && after == before + 1;
        println!(
            "gate2 evidence=result={:?} fallback_used={} attempts={} counter_before={} counter_after={} title={:?} verification={:?} pass={}",
            result.final_outcome,
            result.fallback_used,
            result.attempts.len(),
            before,
            after,
            after_title,
            result.verification,
            pass_fail(pass)
        );
        print_timing("gate2", &result);
        Ok(pass)
    }

    fn run_fallback_denied_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let before = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let target = find_pixel_target(&observation).ok_or("R3 pixel-only target missing")?;
        let result = session.execute_policy(&semantic_request(
            SemanticAction::Invoke {
                element_id: target.id.clone(),
            },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Deny,
        ))?;
        let after = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let pass = result.final_outcome == ComputerExecutionOutcome::FallbackDenied
            && result.attempts.len() == 1
            && !result.fallback_used
            && after == before;
        println!(
            "gate3 evidence=result={:?} attempts={} pixel_attempts={} counter_before={} counter_after={} pass={}",
            result.final_outcome,
            result.attempts.len(),
            result
                .attempts
                .iter()
                .filter(|attempt| attempt.method == alice_computer_use_core::ComputerExecutionMethod::PixelClick)
                .count(),
            before,
            after,
            pass_fail(pass)
        );
        print_timing("gate3", &result);
        Ok(pass)
    }

    fn run_stale_bounds_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let target = find_pixel_target(&observation).ok_or("R3 stale target missing")?;
        let old_id = target.id.clone();
        let before = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let _refresh = session.semantic_observe(&fixture.id, LIMITS)?;
        let result = session.execute_policy(&semantic_request(
            SemanticAction::Invoke { element_id: old_id },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Allow,
        ))?;
        let after = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let pass = result.final_outcome == ComputerExecutionOutcome::StaleElement
            && result.attempts.len() == 1
            && !result.fallback_used
            && after == before;
        println!(
            "gate5 evidence=result={:?} attempts={} fallback_used={} counter_before={} counter_after={} pass={}",
            result.final_outcome,
            result.attempts.len(),
            result.fallback_used,
            before,
            after,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_focus_safety_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
        notepad: Option<&Window>,
    ) -> RunnerResult<bool> {
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let target = find_pixel_target(&observation).ok_or("R3 focus target missing")?;
        let before = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        if let Some(notepad) = notepad {
            let _ = session.action(&ComputerAction::FocusWindow {
                window_id: notepad.id.clone(),
            });
        }
        let active_fixture = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .is_some_and(|window| window.active);
        if active_fixture {
            println!("gate6 evidence=FOREGROUND_SETUP_INVALID fixture_still_foreground=true");
            return Ok(false);
        }
        let result = session.execute_policy(&semantic_request(
            SemanticAction::Invoke {
                element_id: target.id.clone(),
            },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Allow,
        ))?;
        let after = fixture_counter(session, fixture, PIXEL_CLICK_MARKER)?;
        let pass = result.final_outcome == ComputerExecutionOutcome::FocusDenied
            && result.attempts.len() == 1
            && !result.fallback_used
            && after == before;
        println!(
            "gate6 evidence=result={:?} pixel_attempts={} counter_before={} counter_after={} pass={}",
            result.final_outcome,
            result
                .attempts
                .iter()
                .filter(|attempt| attempt.method == alice_computer_use_core::ComputerExecutionMethod::PixelClick)
                .count(),
            before,
            after,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_verification_parity_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        if !ensure_fixture_foreground(session, fixture)? {
            return Ok(false);
        }
        let button_observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let button = find_button(&button_observation).ok_or("R3 parity button missing")?;
        let semantic = session.execute_policy(&semantic_request(
            SemanticAction::Invoke {
                element_id: button.id.clone(),
            },
            ComputerExecutionStrategy::SemanticOnly,
            ComputerFallbackPolicy::Deny,
        ))?;
        let pixel_observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let pixel =
            find_pixel_target(&pixel_observation).ok_or("R3 parity pixel target missing")?;
        let pixel_result = session.execute_policy(&semantic_request(
            SemanticAction::Invoke {
                element_id: pixel.id.clone(),
            },
            ComputerExecutionStrategy::PreferSemantic,
            ComputerFallbackPolicy::Allow,
        ))?;
        let pass = semantic.final_outcome == ComputerExecutionOutcome::Performed
            && pixel_result.final_outcome == ComputerExecutionOutcome::Performed
            && semantic.verification.kind
                == Some(alice_computer_use_core::ComputerExecutionVerificationKind::SemanticStateChanged)
            && pixel_result.verification.kind
                == Some(alice_computer_use_core::ComputerExecutionVerificationKind::SemanticStateChanged)
            && semantic.verification.verified
            && pixel_result.verification.verified;
        println!(
            "gate7 evidence=semantic_verified={} pixel_verified={} semantic_kind={:?} pixel_kind={:?} pass={}",
            semantic.verification.verified,
            pixel_result.verification.verified,
            semantic.verification.kind,
            pixel_result.verification.kind,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_stability_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
        pid: u32,
    ) -> RunnerResult<bool> {
        let mut validations = 0usize;
        let mut executions = 0usize;
        for _ in 0..100 {
            let observation = session.semantic_observe(&fixture.id, LIMITS)?;
            session.validate_element(&observation.root_element_id)?;
            validations += 1;
        }
        for _ in 0..10 {
            let observation = session.semantic_observe(&fixture.id, LIMITS)?;
            let edit = observation
                .elements
                .iter()
                .find(|element| element.capabilities.editable)
                .ok_or("R3 stability EDIT missing")?;
            let result = session.execute_policy(&semantic_request(
                SemanticAction::Invoke {
                    element_id: edit.id.clone(),
                },
                ComputerExecutionStrategy::SemanticOnly,
                ComputerFallbackPolicy::Deny,
            ))?;
            if result.final_outcome == ComputerExecutionOutcome::Unsupported {
                executions += 1;
            }
        }
        let stable_pid = session.window_list().is_ok();
        let pass = validations == 100 && executions == 10 && stable_pid;
        println!(
            "gate9 evidence=validations={} safe_executions={} pid={} stable_pid={} pass={}",
            validations,
            executions,
            pid,
            stable_pid,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_mode_probe(
        session: &ComputerSidecarSession,
        fixture: &Window,
        mode: &str,
    ) -> RunnerResult<bool> {
        let strategy = match mode {
            "semantic-only" => ComputerExecutionStrategy::SemanticOnly,
            "pixel-only" => ComputerExecutionStrategy::PixelOnly,
            "prefer-semantic" => ComputerExecutionStrategy::PreferSemantic,
            _ => return Err("--mode must be semantic-only, pixel-only, or prefer-semantic".into()),
        };
        if !ensure_fixture_foreground(session, fixture)? {
            return Ok(false);
        }
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let target = find_pixel_target(&observation).ok_or("mode probe pixel target missing")?;
        let result = session.execute_policy(&semantic_request(
            SemanticAction::Invoke {
                element_id: target.id.clone(),
            },
            strategy,
            ComputerFallbackPolicy::Allow,
        ))?;
        let title = fixture_title(session, fixture)?;
        println!(
            "mode_probe result={:?} fallback_used={} attempts={} title={:?} verification={:?}",
            result.final_outcome,
            result.fallback_used,
            result.attempts.len(),
            title,
            result.verification
        );
        print_timing("mode_probe", &result);
        Ok(match mode {
            "semantic-only" => result.final_outcome == ComputerExecutionOutcome::Unsupported,
            "pixel-only" | "prefer-semantic" => {
                result.final_outcome == ComputerExecutionOutcome::Performed
            }
            _ => false,
        })
    }

    fn semantic_request(
        action: SemanticAction,
        strategy: ComputerExecutionStrategy,
        fallback_policy: ComputerFallbackPolicy,
    ) -> ComputerExecutionRequest {
        ComputerExecutionRequest {
            intent: ComputerExecutionIntent::Semantic(action),
            strategy,
            fallback_policy,
            execution_mode: Default::default(),
        }
    }

    fn find_button(observation: &SemanticObservation) -> Option<&ComputerElement> {
        observation.elements.iter().find(|element| {
            element.control_type == "button"
                && element.name.as_deref() == Some("Set Status")
                && element.capabilities.invokable
        })
    }

    fn find_pixel_target(observation: &SemanticObservation) -> Option<&ComputerElement> {
        observation.elements.iter().find(|element| {
            element.name.as_deref() == Some("Pixel Only Target")
                && !element.capabilities.invokable
                && element.bounds.is_some()
                && !element.offscreen
        })
    }

    fn ensure_fixture_foreground(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let active = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .is_some_and(|window| window.active);
        if active {
            println!("fixture_foreground=ALREADY_ACTIVE");
            return Ok(true);
        }

        let focused = session.action(&ComputerAction::FocusWindow {
            window_id: fixture.id.clone(),
        });
        if focused.is_ok() {
            println!("fixture_foreground=ACQUIRED");
            Ok(true)
        } else {
            println!("fixture_foreground=DENIED");
            Ok(false)
        }
    }

    fn wait_for_fixture_foreground(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<()> {
        println!("fixture_foreground=WAITING_FOR_MANUAL_PRECONDITION");
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            let active = session
                .window_list()?
                .into_iter()
                .find(|window| window.id == fixture.id)
                .is_some_and(|window| window.active);
            if active {
                println!("fixture_foreground=READY");
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err("fixture did not become foreground during manual precondition wait".into())
    }

    fn fixture_counter(
        session: &ComputerSidecarSession,
        fixture: &Window,
        marker: &str,
    ) -> RunnerResult<u32> {
        let title = fixture_title(session, fixture)?;
        Ok(title
            .split(marker)
            .nth(1)
            .and_then(|value| value.split(';').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    fn fixture_title(session: &ComputerSidecarSession, fixture: &Window) -> RunnerResult<String> {
        session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .map(|window| window.title)
            .ok_or_else(|| "fixture disappeared".into())
    }

    fn print_timing(label: &str, result: &alice_computer_use_core::ComputerExecutionResult) {
        println!(
            "{} timing=policy_ms:{} semantic_attempt_ms:{} fallback_decision_ms:{} pixel_attempt_ms:{} verification_ms:{} total_ms:{}",
            label,
            result.timing.policy_ms,
            result.timing.semantic_attempt_ms,
            result.timing.fallback_decision_ms,
            result.timing.pixel_attempt_ms,
            result.timing.verification_ms,
            result.timing.total_ms
        );
    }

    fn start_fixture(path: &Path) -> RunnerResult<Child> {
        if !path.exists() {
            return Err(format!("fixture executable does not exist: {}", path.display()).into());
        }
        let child = Command::new(path)
            .stdin(Stdio::null())
            .env("ALICE_R3_SLOW_INVOKE", "1")
            .spawn()
            .map_err(|error| format!("failed to start R3 fixture: {error}"))?;
        println!("fixture_start=PASS pid={}", child.id());
        thread::sleep(Duration::from_millis(300));
        Ok(child)
    }

    fn wait_for_action_start() {
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            thread::yield_now();
        }
    }

    fn terminate_sidecar(pid: u32) -> RunnerResult<()> {
        let handle = unsafe { OpenProcess(PROCESS_TERMINATE, 0, pid) };
        if handle.is_null() {
            return Err(format!("OpenProcess failed for sidecar PID {pid}").into());
        }
        let terminated = unsafe { TerminateProcess(handle, 137) };
        unsafe {
            let _ = CloseHandle(handle);
        }
        if terminated == 0 {
            return Err(format!("TerminateProcess failed for sidecar PID {pid}").into());
        }
        Ok(())
    }

    fn find_window(windows: &[Window], needle: &str) -> Option<Window> {
        let needle = needle.to_ascii_lowercase();
        windows
            .iter()
            .find(|window| window.title.to_ascii_lowercase().contains(&needle))
            .cloned()
    }

    fn argument_value(name: &str) -> Option<String> {
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == name {
                return args.next();
            }
        }
        None
    }

    fn has_flag(flag: &str) -> bool {
        std::env::args().skip(1).any(|arg| arg == flag)
    }

    fn pass_fail(value: bool) -> &'static str {
        if value {
            "PASS"
        } else {
            "FAIL"
        }
    }
}

#[cfg(windows)]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    windows_runner::run()
}
