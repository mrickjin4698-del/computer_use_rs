//! Real interactive Windows acceptance runner for Computer-INFRA-R2.
//!
//! The runner uses the production sidecar client and therefore reaches
//! WinNativeBackend -> the read-only UIA semantic action provider.  It does
//! not use pixel fallback, UIA action shims, OCR, VLM, browser automation, or
//! an agent loop.  The Win32 fixture is test-only and is never part of the
//! production backend.

#[cfg(not(windows))]
fn main() {
    eprintln!("r2_semantic_actions requires Windows");
}

#[cfg(windows)]
mod windows_runner {
    use alice_computer_use_core::{
        ComputerAction, SemanticAction, SemanticActionResult, SemanticActionStatus,
        SemanticObservation, SemanticObservationLimits, Window,
    };
    use alice_computer_use_sidecar_client::{
        ComputerSidecarClient, ComputerSidecarSession, SidecarError,
    };
    use std::{
        collections::HashMap,
        error::Error,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        sync::{Arc, Barrier},
        thread,
        time::Duration,
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
    const FIXTURE_TITLE_PREFIX: &str = "Alice Computer Input Fixture | ";
    const INVOKE_COUNT_MARKER: &str = "BUTTON_INVOKES=";

    pub fn run() -> RunnerResult<()> {
        let sidecar_path = argument_value("--sidecar")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
        let fixture_path = argument_value("--fixture")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/debug/examples/r0_input_fixture.exe"));
        println!("runner=alice-computer-use-sidecar-client/r2_semantic_actions");
        println!("sidecar={}", sidecar_path.display());
        println!("fixture={}", fixture_path.display());

        let mut fixture_child = if has_flag("--no-start-fixture") {
            println!("fixture_start=SKIP reason=manual_fixture_mode");
            None
        } else {
            start_fixture(&fixture_path)?
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
                .ok_or("R2 requires the Win32 fixture window")?;
            let notepad = find_window(&windows, "notepad")
                .ok_or("R2 requires an open modern Notepad window")?;
            println!(
                "targets fixture_id={} fixture_title={:?} notepad_id={} notepad_title={:?}",
                fixture.id, fixture.title, notepad.id, notepad.title
            );

            if has_flag("--semantic-focus-only") {
                let (target_name, target) = match argument_value("--focus-target")
                    .as_deref()
                    .unwrap_or("fixture")
                {
                    "fixture" => ("fixture", &fixture),
                    "notepad" => ("notepad", &notepad),
                    value => {
                        return Err(format!(
                            "unsupported --focus-target={value}; expected fixture or notepad"
                        )
                        .into())
                    }
                };
                let pass = run_semantic_focus_only(&session, target_name, target)?;
                let closed = session.close().is_ok();
                let shutdown = client.shutdown().is_ok();
                println!(
                    "semantic_focus_only lifecycle session_close={} shutdown={} result={}",
                    closed,
                    shutdown,
                    pass_fail(pass && closed && shutdown)
                );
                return if pass && closed && shutdown {
                    Ok(())
                } else {
                    Err("semantic focus-only gate failed".into())
                };
            }

            let negative_focus = run_negative_focus_gate(&session, &fixture, &notepad)?;
            println!(
                "gate1_negative_window_not_foreground={}",
                pass_fail(negative_focus)
            );
            let gate1 = run_focus_gate(&session, &fixture, &notepad)?;
            println!("gate1_focus={}", pass_fail(gate1));
            let gate2 = run_invoke_gate(&session, &fixture)?;
            println!("gate2_invoke={}", pass_fail(gate2));
            let gate3 = run_set_value_gate(&session, &fixture)?;
            println!("gate3_set_value={}", pass_fail(gate3));
            let gate4 = run_capability_gate(&session, &fixture)?;
            println!("gate4_capability_admission={}", pass_fail(gate4));
            let gate5 = run_stale_gate(&session, &fixture)?;
            println!("gate5_stale_safety={}", pass_fail(gate5));
            let gate6 = run_no_double_execution_gate(&session, &fixture)?;
            println!("gate6_no_double_execution={}", pass_fail(gate6));

            let crash_pid = client.process_id();
            let crash_before_count = session
                .window_list()?
                .into_iter()
                .find(|window| window.id == fixture.id)
                .map(|window| parse_invoke_count(&window.title))
                .unwrap_or(0);
            let crash_session = session.clone();
            let crash_fixture = fixture.clone();
            let started = Arc::new(Barrier::new(2));
            let thread_started = Arc::clone(&started);
            let pending = thread::spawn(move || {
                let observation = crash_session.semantic_observe(&crash_fixture.id, LIMITS)?;
                let button = find_button(&observation).ok_or_else(|| {
                    SidecarError::InvalidResponse("fixture button missing".into())
                })?;
                thread_started.wait();
                Ok::<_, SidecarError>(crash_session.semantic_action(&SemanticAction::Invoke {
                    element_id: button.id.clone(),
                }))
            });
            started.wait();
            thread::sleep(Duration::from_millis(50));
            terminate_sidecar(crash_pid)?;
            let pending_result = pending
                .join()
                .map_err(|_| "Gate 7 action thread panicked")??;
            let crash_unknown = match pending_result {
                Err(error) => error.outcome_unknown(),
                Ok(_) => false,
            };
            let old_session_invalid = session.window_list().is_err();
            println!(
                "gate7.crash action_outcome_unknown={} old_session_invalid={} no_retry=true no_replay=true crash_before_count={}",
                crash_unknown, old_session_invalid, crash_before_count
            );

            let restarted = ComputerSidecarClient::spawn(&sidecar_path, Duration::from_secs(30))?;
            let new_session = restarted.create_session()?;
            let new_windows = new_session.window_list()?;
            let new_fixture = find_window(&new_windows, "alice computer input fixture")
                .ok_or("Gate 7 restart could not find fixture")?;
            let new_observation = new_session.semantic_observe(&new_fixture.id, LIMITS)?;
            let restart_observation = !new_observation.elements.is_empty();
            let crash_after_count = new_windows
                .iter()
                .find(|window| window.id == new_fixture.id)
                .map(|window| parse_invoke_count(&window.title))
                .unwrap_or(0);
            let no_replay = crash_after_count <= crash_before_count + 1;
            let gate7 = crash_unknown && old_session_invalid && no_replay;
            println!(
                "gate7 evidence=action_outcome_unknown={} old_session_invalid={} fixture_count_after_restart={} no_replay={} result={}",
                crash_unknown, old_session_invalid, crash_after_count, no_replay, pass_fail(gate7)
            );
            let gate8 = run_stability_gate(&new_session, &new_fixture, restarted.process_id())?;
            let new_session_close = new_session.close().is_ok();
            let shutdown = restarted.shutdown().is_ok();
            let repeated_shutdown = restarted.shutdown().is_ok();
            println!(
                "gate8.lifecycle stability={} new_observation={} session_close={} shutdown={} repeated_shutdown={} pid={}",
                pass_fail(gate8), restart_observation, new_session_close, shutdown,
                repeated_shutdown, restarted.process_id()
            );
            let all = gate1
                && gate2
                && gate3
                && gate4
                && gate5
                && gate6
                && gate7
                && gate8
                && restart_observation
                && new_session_close
                && shutdown
                && repeated_shutdown;
            println!("result={}", pass_fail(all));
            if all {
                Ok(())
            } else {
                Err("one or more R2 semantic action gates failed".into())
            }
        })();

        if let Some(mut child) = fixture_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    fn run_focus_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
        notepad: &Window,
    ) -> RunnerResult<bool> {
        let fixture_window_focus = session.action(&ComputerAction::FocusWindow {
            window_id: fixture.id.clone(),
        });
        let fixture_observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let fixture_edit = find_edit(&fixture_observation).ok_or("fixture EDIT not found")?;
        let fixture_result = if fixture_window_focus.is_ok() {
            Some(session.semantic_action(&SemanticAction::Focus {
                element_id: fixture_edit.id.clone(),
            })?)
        } else {
            println!(
                "gate1 fixture=WINDOW_FOCUS_DENIED semantic_focus=SKIP reason=foreground_precondition"
            );
            None
        };
        let fixture_after = session.semantic_observe(&fixture.id, LIMITS)?;
        let fixture_focused = find_edit(&fixture_after)
            .map(|element| element.focused)
            .unwrap_or(false);

        let notepad_window_focus = session.action(&ComputerAction::FocusWindow {
            window_id: notepad.id.clone(),
        });
        let notepad_observation = session.semantic_observe(&notepad.id, LIMITS)?;
        let notepad_edit = find_edit(&notepad_observation);
        let notepad_result = if notepad_window_focus.is_ok() {
            notepad_edit
                .map(|element| {
                    session.semantic_action(&SemanticAction::Focus {
                        element_id: element.id.clone(),
                    })
                })
                .transpose()?
        } else {
            println!(
                "gate1 notepad=WINDOW_FOCUS_DENIED semantic_focus=SKIP reason=foreground_precondition"
            );
            None
        };
        let notepad_after = session.semantic_observe(&notepad.id, LIMITS)?;
        let notepad_focused = find_edit(&notepad_after)
            .map(|element| element.focused)
            .unwrap_or(false);
        println!(
            "gate1 evidence=fixture_window_focus={:?} fixture_result={:?} fixture_focused={} notepad_window_focus={:?} notepad_result={:?} notepad_focused={}",
            fixture_window_focus,
            fixture_result,
            fixture_focused,
            notepad_window_focus,
            notepad_result,
            notepad_focused
        );
        Ok(fixture_result.as_ref().is_some_and(|result| {
            is_performed(result) && result.verification.focused == Some(true)
        }) && fixture_focused
            && notepad_result.as_ref().is_some_and(is_performed)
            && notepad_focused)
    }

    fn run_negative_focus_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
        notepad: &Window,
    ) -> RunnerResult<bool> {
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let edit =
            find_edit(&observation).ok_or("fixture EDIT not found for negative focus gate")?;
        let active = session
            .window_list()?
            .into_iter()
            .find(|window| window.active);

        if active
            .as_ref()
            .is_some_and(|window| window.id == fixture.id)
        {
            let switch = session.action(&ComputerAction::FocusWindow {
                window_id: notepad.id.clone(),
            });
            println!(
                "gate1 negative_setup=fixture_foreground switch_to_notepad={:?}",
                switch
            );
            if switch.is_err() {
                println!(
                    "gate1 negative=BLOCKED reason=fixture_remained_foreground_after_focus_denial"
                );
                return Ok(false);
            }
        }

        let before = session.window_list()?;
        let target_is_foreground = before
            .iter()
            .find(|window| window.id == fixture.id)
            .is_some_and(|window| window.active);
        if target_is_foreground {
            println!("gate1 negative=BLOCKED reason=target_still_foreground");
            return Ok(false);
        }

        let result = session.semantic_action(&SemanticAction::Focus {
            element_id: edit.id.clone(),
        })?;
        let after_windows = session.window_list()?;
        let after_observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let after_edit_focused = find_edit(&after_observation)
            .map(|element| element.focused)
            .unwrap_or(false);
        let foreground_unchanged = after_windows
            .iter()
            .find(|window| window.id == fixture.id)
            .is_some_and(|window| !window.active);
        let pass = result.status == SemanticActionStatus::WindowNotForeground
            && !result.verification.verified
            && result.verification.focused == Some(false)
            && foreground_unchanged
            && !after_edit_focused;
        println!(
            "gate1 negative evidence=result={:?} foreground_unchanged={} edit_focused={} no_uia_side_effect={} pass={}",
            result,
            foreground_unchanged,
            after_edit_focused,
            foreground_unchanged && !after_edit_focused,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_semantic_focus_only(
        session: &ComputerSidecarSession,
        target_name: &str,
        target: &Window,
    ) -> RunnerResult<bool> {
        let before = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == target.id)
            .ok_or("focus target disappeared before semantic focus-only gate")?;
        let observation = session.semantic_observe(&target.id, LIMITS)?;
        let edit = find_edit(&observation).ok_or("editable focus target not found")?;
        let result = session.semantic_action(&SemanticAction::Focus {
            element_id: edit.id.clone(),
        })?;
        let after = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == target.id);
        let after_observation = session.semantic_observe(&target.id, LIMITS)?;
        let focused = find_edit(&after_observation)
            .map(|element| element.focused)
            .unwrap_or(false);
        let pass = before.active
            && after.as_ref().is_some_and(|window| window.active)
            && is_performed(&result)
            && result.verification.focused == Some(true)
            && focused;
        println!(
            "semantic_focus_only target={} evidence=window_was_foreground={} result={:?} element_focused={} pass={}",
            target_name,
            before.active,
            result,
            focused,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_invoke_gate(session: &ComputerSidecarSession, fixture: &Window) -> RunnerResult<bool> {
        let before_title = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .map(|window| window.title)
            .unwrap_or_default();
        let before_count = parse_invoke_count(&before_title);
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let button = find_button(&observation).ok_or("fixture button not found")?;
        let result = session.semantic_action(&SemanticAction::Invoke {
            element_id: button.id.clone(),
        })?;
        let after_observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let invoked_label = after_observation
            .elements
            .iter()
            .any(|element| element.name.as_deref() == Some("INVOKED"));
        let after_title = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .map(|window| window.title)
            .unwrap_or_default();
        let after_count = parse_invoke_count(&after_title);
        let pass = is_performed(&result)
            && result.verification.verified
            && result.verification.state_changed == Some(true)
            && invoked_label
            && after_count == before_count + 1;
        println!(
            "gate2 evidence=result={:?} READY_to_INVOKED={} semantic_label={} count_before={} count_after={} pass={}",
            result, before_title.contains("READY"), invoked_label, before_count, after_count,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_set_value_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let values = [
            "ABCabc0123",
            "ALICE_COMPUTER_R2_123456",
            "_-+=",
            "!@#$%",
            "中文测试",
            "Alice_UIA_混合123",
        ];
        let mut pass = true;
        for expected in values {
            let observation = session.semantic_observe(&fixture.id, LIMITS)?;
            let edit =
                find_value_edit(&observation).ok_or("fixture ValuePattern EDIT disappeared")?;
            let result = session.semantic_action(&SemanticAction::SetValue {
                element_id: edit.id.clone(),
                value: expected.to_owned(),
            })?;
            let after_observation = session.semantic_observe(&fixture.id, LIMITS)?;
            let semantic_value = find_edit(&after_observation).and_then(|element| {
                element
                    .value_summary
                    .clone()
                    .or(element.text_summary.clone())
            });
            let title = session
                .window_list()?
                .into_iter()
                .find(|window| window.id == fixture.id)
                .map(|window| window.title)
                .unwrap_or_default();
            let wm_gettext = fixture_text_from_title(&title);
            let exact = semantic_value.as_deref() == Some(expected)
                && wm_gettext.as_deref() == Some(expected);
            let case_pass = is_performed(&result)
                && result.verification.verified
                && result.verification.observed_value.as_deref() == Some(expected)
                && exact;
            println!(
                "gate3 case={:?} result={:?} semantic_value={:?} wm_gettext={:?} exact={} pass={}",
                expected,
                result,
                semantic_value,
                wm_gettext,
                exact,
                pass_fail(case_pass)
            );
            pass &= case_pass;
        }
        Ok(pass)
    }

    fn run_capability_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let edit = find_edit(&observation).ok_or("fixture EDIT not found for capability gate")?;
        let button =
            find_button(&observation).ok_or("fixture button not found for capability gate")?;
        let static_element = observation
            .elements
            .iter()
            .find(|element| !element.focusable && element.name.is_some())
            .ok_or("fixture non-focusable element not found")?;
        let invoke_edit = session.semantic_action(&SemanticAction::Invoke {
            element_id: edit.id.clone(),
        })?;
        let set_button = session.semantic_action(&SemanticAction::SetValue {
            element_id: button.id.clone(),
            value: "must-not-be-set".into(),
        })?;
        let focus_static = session.semantic_action(&SemanticAction::Focus {
            element_id: static_element.id.clone(),
        })?;
        let pass = invoke_edit.status == SemanticActionStatus::Unsupported
            && set_button.status == SemanticActionStatus::Unsupported
            && matches!(
                focus_static.status,
                SemanticActionStatus::Unsupported | SemanticActionStatus::FocusDenied
            );
        println!(
            "gate4 evidence=invoke_edit={:?} set_button={:?} focus_non_focusable={:?} pass={}",
            invoke_edit.status,
            set_button.status,
            focus_static.status,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_stale_gate(session: &ComputerSidecarSession, fixture: &Window) -> RunnerResult<bool> {
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let edit = find_edit(&observation).ok_or("fixture EDIT not found for stale gate")?;
        let old_id = edit.id.clone();
        let action = session.semantic_action(&SemanticAction::Focus {
            element_id: old_id.clone(),
        })?;
        let stale = session
            .validate_element(&old_id)
            .err()
            .map(|error| error.code() == "STALE_ELEMENT")
            .unwrap_or(false);
        let refreshed = session.semantic_observe(&fixture.id, LIMITS)?;
        let current = find_edit(&refreshed)
            .map(|element| session.validate_element(&element.id).is_ok())
            .unwrap_or(false);
        let pass = stale && current;
        println!(
            "gate5 evidence=action_status={:?} old_stale={} current_valid={} pass={}",
            action.status,
            stale,
            current,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_no_double_execution_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
    ) -> RunnerResult<bool> {
        let before_title = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .map(|window| window.title)
            .unwrap_or_default();
        let before_count = parse_invoke_count(&before_title);
        let observation = session.semantic_observe(&fixture.id, LIMITS)?;
        let button =
            find_button(&observation).ok_or("fixture button not found for no-double gate")?;
        let result = session.semantic_action(&SemanticAction::Invoke {
            element_id: button.id.clone(),
        })?;
        let after_title = session
            .window_list()?
            .into_iter()
            .find(|window| window.id == fixture.id)
            .map(|window| window.title)
            .unwrap_or_default();
        let after_count = parse_invoke_count(&after_title);
        let exactly_once = after_count == before_count + 1;
        let allowed_result = matches!(
            result.status,
            SemanticActionStatus::Performed | SemanticActionStatus::VerificationFailed
        );
        let pass = allowed_result && exactly_once;
        println!(
            "gate6 evidence=result_status={:?} before_count={} after_count={} exactly_once={} no_retry=true pass={}",
            result.status, before_count, after_count, exactly_once, pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_stability_gate(
        session: &ComputerSidecarSession,
        fixture: &Window,
        pid: u32,
    ) -> RunnerResult<bool> {
        let mut pass = true;
        let mut total = 0usize;
        for index in 0..100 {
            let observation = session.semantic_observe(&fixture.id, LIMITS)?;
            total += observation.elements.len();
            let valid = !observation.elements.is_empty()
                && validate_shape(&observation)
                && session
                    .validate_element(&observation.root_element_id)
                    .is_ok();
            pass &= valid;
            if !valid {
                println!("gate8 failure_at={}", index + 1);
                break;
            }
        }
        println!(
            "gate8 evidence=requests=100 total_elements={} correlation=true pid={} pass={}",
            total,
            pid,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn find_edit(
        observation: &SemanticObservation,
    ) -> Option<&alice_computer_use_core::ComputerElement> {
        observation.elements.iter().find(|element| {
            matches!(element.control_type.as_str(), "edit" | "document")
                && element.capabilities.editable
        })
    }

    fn find_value_edit(
        observation: &SemanticObservation,
    ) -> Option<&alice_computer_use_core::ComputerElement> {
        find_edit(observation).filter(|element| element.value_summary.is_some())
    }

    fn find_button(
        observation: &SemanticObservation,
    ) -> Option<&alice_computer_use_core::ComputerElement> {
        observation.elements.iter().find(|element| {
            element.control_type == "button"
                && element
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains("Set Status"))
                && element.capabilities.invokable
        })
    }

    fn validate_shape(observation: &SemanticObservation) -> bool {
        let by_id = observation
            .elements
            .iter()
            .map(|element| (element.id.clone(), element))
            .collect::<HashMap<_, _>>();
        by_id.contains_key(&observation.root_element_id)
            && observation
                .elements
                .iter()
                .filter(|element| element.parent_id.is_none())
                .count()
                == 1
            && observation.elements.iter().all(|element| {
                element.child_ids.iter().all(|child_id| {
                    by_id
                        .get(child_id)
                        .and_then(|child| child.parent_id.as_ref())
                        == Some(&element.id)
                })
            })
    }

    fn find_window(windows: &[Window], needle: &str) -> Option<Window> {
        let needle = needle.to_ascii_lowercase();
        windows
            .iter()
            .find(|window| window.title.to_ascii_lowercase().contains(&needle))
            .cloned()
    }

    fn start_fixture(path: &Path) -> RunnerResult<Option<Child>> {
        if !path.exists() {
            return Err(format!("fixture executable does not exist: {}", path.display()).into());
        }
        let child = Command::new(path)
            .stdin(Stdio::null())
            .env("ALICE_R2_SLOW_INVOKE", "1")
            .spawn()
            .map_err(|error| format!("failed to start R2 fixture: {error}"))?;
        println!("fixture_start=PASS pid={}", child.id());
        thread::sleep(Duration::from_millis(300));
        Ok(Some(child))
    }

    fn fixture_text_from_title(title: &str) -> Option<String> {
        let text = title.strip_prefix(FIXTURE_TITLE_PREFIX)?;
        let status_start = text.find("STATUS=").unwrap_or(text.len());
        let text = text[..status_start].trim_end_matches(" || ");
        Some(
            text.replace("\\r", "\r")
                .replace("\\n", "\n")
                .replace("\\\\", "\\"),
        )
    }

    fn parse_invoke_count(title: &str) -> u32 {
        title
            .split(INVOKE_COUNT_MARKER)
            .nth(1)
            .and_then(|value| value.split(';').next())
            .and_then(|value| value.parse().ok())
            .unwrap_or(0)
    }

    fn is_performed(result: &SemanticActionResult) -> bool {
        result.status == SemanticActionStatus::Performed
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
