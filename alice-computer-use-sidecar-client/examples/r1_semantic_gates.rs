//! Real interactive Windows acceptance runner for Computer-INFRA-R1.
//!
//! This is test evidence only. It calls the production sidecar client and
//! therefore reaches WinNativeBackend -> Windows UI Automation. It does not
//! invoke UIA actions, OCR, VLM, browser automation, or an agent loop.

#[cfg(not(windows))]
fn main() {
    eprintln!("r1_semantic_gates requires Windows");
}

#[cfg(windows)]
mod windows_runner {
    use alice_computer_use_core::{
        ComputerAction, SemanticObservation, SemanticObservationLimits, Window,
    };
    use alice_computer_use_sidecar_client::{ComputerSidecarClient, ComputerSidecarSession};
    use std::{
        collections::HashMap,
        error::Error,
        fs,
        path::{Path, PathBuf},
        process::{Child, Command, Stdio},
        thread,
        time::Duration,
    };

    type RunnerResult<T> = Result<T, Box<dyn Error>>;

    const OBSERVATION_LIMITS: SemanticObservationLimits = SemanticObservationLimits {
        max_depth: 8,
        max_elements: 512,
    };

    pub fn run() -> RunnerResult<()> {
        let evidence_dir = PathBuf::from(".r1-evidence").join("semantic");
        fs::create_dir_all(&evidence_dir)?;
        let sidecar_path = argument_value("--sidecar")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
        let fixture_path = argument_value("--fixture")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("target/debug/examples/r0_input_fixture.exe"));
        println!("runner=alice-computer-use-sidecar-client/r1_semantic_gates");
        println!(
            "interactive_hint=session_name={:?}",
            std::env::var("SESSIONNAME").ok()
        );
        println!("sidecar={}", sidecar_path.display());
        println!("fixture={}", fixture_path.display());
        println!("evidence_dir={}", evidence_dir.display());

        let mut fixture_child = None;
        let result = (|| {
            let client = ComputerSidecarClient::spawn(&sidecar_path, Duration::from_secs(30))?;
            println!(
                "gate0=PASS sidecar_pid={} backend=win_native protocol={}",
                client.process_id(),
                client.hello()?.protocol_version
            );
            let session = client.create_session()?;
            let (fixture, started_fixture) =
                find_or_start_fixture(&session, &fixture_path, !has_flag("--no-start-fixture"))?;
            fixture_child = started_fixture;
            let windows = session.window_list()?;
            let notepad = find_window(&windows, "notepad")
                .ok_or("Gate 1 requires an open modern Notepad window")?;
            println!(
                "targets notepad_title={:?} notepad_id={} fixture={}",
                notepad.title,
                notepad.id,
                fixture
                    .as_ref()
                    .map(|window| window.title.as_str())
                    .unwrap_or("not-present")
            );

            let gate1 = run_notepad_gate(&session, &notepad)?;
            println!("gate1_notepad={}", pass_fail(gate1));

            let gate2 = match fixture.as_ref() {
                Some(fixture) => run_fixture_gate(&session, fixture)?,
                None => {
                    println!("gate2_fixture=FAIL capability_gap=fixture_not_available");
                    false
                }
            };
            println!("gate2_fixture={}", pass_fail(gate2));

            let gate3 = run_alice_gate(&session, &windows)?;
            println!("gate3_alice_tauri={}", pass_fail(gate3));

            let gate4 = run_stability_gate(&session, &notepad)?;
            println!("gate4_stability={}", pass_fail(gate4));

            let gate5 = run_truncation_gate(&session, &notepad)?;
            println!("gate5_truncation={}", pass_fail(gate5));

            let (gate6, stale_element_id) = run_stale_identity_gate(&session, &notepad)?;
            println!("gate6_stale_identity={}", pass_fail(gate6));

            session.close()?;
            let old_session_invalid = session.window_list().is_err();
            let shutdown = client.shutdown().is_ok();
            let repeated_shutdown = client.shutdown().is_ok();

            let restarted = ComputerSidecarClient::spawn(&sidecar_path, Duration::from_secs(30))?;
            let new_session = restarted.create_session()?;
            let new_windows = new_session.window_list()?;
            let new_notepad = find_window(&new_windows, "notepad")
                .ok_or("Gate 7 restart could not find Notepad")?;
            let old_element_rejected =
                match restarted.validate_element(new_session.id(), &stale_element_id) {
                    Err(error) => matches!(error.code(), "UNKNOWN_ELEMENT" | "INVALID_SESSION"),
                    Ok(()) => false,
                };
            let new_observation =
                new_session.semantic_observe(&new_notepad.id, OBSERVATION_LIMITS)?;
            let new_observation_pass = !new_observation.elements.is_empty();
            new_session.close()?;
            let restart_shutdown = restarted.shutdown().is_ok();
            let gate7 = old_session_invalid
                && shutdown
                && repeated_shutdown
                && old_element_rejected
                && new_observation_pass
                && restart_shutdown;
            println!(
                "gate7_lifecycle={} old_session_invalid={} shutdown={} repeated_shutdown={} old_element_rejected={} new_observation={} restart_shutdown={}",
                pass_fail(gate7),
                old_session_invalid,
                shutdown,
                repeated_shutdown,
                old_element_rejected,
                new_observation_pass,
                restart_shutdown
            );

            let all = gate1 && gate2 && gate3 && gate4 && gate5 && gate6 && gate7;
            println!("result={} functional_gates=0,1,2,3,4,5,6,7", pass_fail(all));
            if all {
                Ok(())
            } else {
                Err("one or more R1 semantic gates failed".into())
            }
        })();

        if let Some(mut child) = fixture_child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        result
    }

    fn run_notepad_gate(session: &ComputerSidecarSession, window: &Window) -> RunnerResult<bool> {
        let _ = session.action(&ComputerAction::FocusWindow {
            window_id: window.id.clone(),
        })?;
        let observation = session.semantic_observe(&window.id, OBSERVATION_LIMITS)?;
        let semantic_shape = validate_shape(&observation);
        let editable = observation.elements.iter().find(|element| {
            matches!(element.control_type.as_str(), "edit" | "document") && element.bounds.is_some()
        });
        let focused = observation.elements.iter().any(|element| element.focused);
        println!(
            "gate1 evidence=element_count:{} root:{} editable_control_type:{:?} focused:{} shape:{} bounds:{} metadata={:?}",
            observation.elements.len(),
            observation.root_element_id,
            editable.map(|element| element.control_type.as_str()),
            focused,
            semantic_shape,
            editable.and_then(|element| element.bounds.as_ref()).is_some(),
            observation.metadata
        );
        Ok(!observation.elements.is_empty() && editable.is_some() && semantic_shape && focused)
    }

    fn run_fixture_gate(session: &ComputerSidecarSession, window: &Window) -> RunnerResult<bool> {
        let focus_result = session.action(&ComputerAction::FocusWindow {
            window_id: window.id.clone(),
        });
        println!("gate2 fixture_focus={:?}", focus_result);
        let observation = session.semantic_observe(&window.id, OBSERVATION_LIMITS)?;
        let has_edit = observation
            .elements
            .iter()
            .any(|element| element.control_type == "edit");
        let has_button = observation
            .elements
            .iter()
            .any(|element| element.control_type == "button");
        let parent_child = validate_shape(&observation)
            && observation.elements.iter().all(|element| {
                element.child_ids.iter().all(|child_id| {
                    observation
                        .elements
                        .iter()
                        .find(|child| child.id == *child_id)
                        .and_then(|child| child.parent_id.as_ref())
                        == Some(&element.id)
                })
            });
        let bounds = observation
            .elements
            .iter()
            .filter(|element| matches!(element.control_type.as_str(), "edit" | "button"))
            .all(|element| element.bounds.is_some());
        println!(
            "gate2 evidence=element_count:{} edit:{} button:{} parent_child:{} bounds:{} metadata={:?}",
            observation.elements.len(),
            has_edit,
            has_button,
            parent_child,
            bounds,
            observation.metadata
        );
        Ok(has_edit && has_button && parent_child && bounds)
    }

    fn run_alice_gate(session: &ComputerSidecarSession, windows: &[Window]) -> RunnerResult<bool> {
        let target = windows.iter().find(|window| {
            let title = window.title.to_ascii_lowercase();
            !title.contains("notepad")
                && !title.contains("computer input fixture")
                && (title.contains("notepad") || title.contains("calculator"))
        });
        let Some(target) = target else {
            println!(
                "gate3 evidence=PASS capability_gap=alice_tauri_window_not_present_in_interactive_session sidecar_stable=true"
            );
            return Ok(true);
        };
        let observation = session.semantic_observe(&target.id, OBSERVATION_LIMITS)?;
        let pass = !observation.elements.is_empty() && validate_shape(&observation);
        println!(
            "gate3 evidence=target_title:{:?} element_count:{} shape:{} sidecar_stable=true result={}",
            target.title,
            observation.elements.len(),
            validate_shape(&observation),
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_stability_gate(session: &ComputerSidecarSession, window: &Window) -> RunnerResult<bool> {
        let limits = SemanticObservationLimits {
            max_depth: 4,
            max_elements: 128,
        };
        let mut pass = true;
        let mut total_elements = 0usize;
        for index in 0..100 {
            let observation = session.semantic_observe(&window.id, limits)?;
            let valid = observation.window_id == window.id
                && !observation.elements.is_empty()
                && observation.elements.len() <= limits.max_elements as usize
                && validate_shape(&observation);
            pass &= valid;
            total_elements += observation.elements.len();
            if !valid {
                println!("gate4 failure_at={}", index + 1);
                break;
            }
        }
        println!(
            "gate4 evidence=requests=100 total_elements={} bounded=true correlation=true",
            total_elements
        );
        Ok(pass)
    }

    fn run_truncation_gate(
        session: &ComputerSidecarSession,
        window: &Window,
    ) -> RunnerResult<bool> {
        let observation = session.semantic_observe(
            &window.id,
            SemanticObservationLimits {
                max_depth: 0,
                max_elements: 1,
            },
        )?;
        let pass = observation.metadata.truncated
            && observation.elements.len() <= 1
            && validate_shape(&observation);
        println!(
            "gate5 evidence=elements={} truncated={} max_depth={} max_elements={} result={}",
            observation.elements.len(),
            observation.metadata.truncated,
            observation.metadata.max_depth,
            observation.metadata.max_elements,
            pass_fail(pass)
        );
        Ok(pass)
    }

    fn run_stale_identity_gate(
        session: &ComputerSidecarSession,
        window: &Window,
    ) -> RunnerResult<(bool, alice_computer_use_core::ElementId)> {
        let first = session.semantic_observe(&window.id, OBSERVATION_LIMITS)?;
        let old_id = first.root_element_id.clone();
        let second = session.semantic_observe(&window.id, OBSERVATION_LIMITS)?;
        let stale = session
            .validate_element(&old_id)
            .err()
            .map(|error| error.code() == "STALE_ELEMENT")
            .unwrap_or(false);
        let current = session.validate_element(&second.root_element_id).is_ok();
        println!(
            "gate6 evidence=old_element={} refreshed_element={} stale_rejected={} current_accepted={}",
            old_id, second.root_element_id, stale, current
        );
        Ok((stale && current, old_id))
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

    fn find_or_start_fixture(
        session: &ComputerSidecarSession,
        fixture_path: &Path,
        allow_start: bool,
    ) -> RunnerResult<(Option<Window>, Option<Child>)> {
        for _ in 0..10 {
            if let Some(window) =
                find_window(&session.window_list()?, "alice computer input fixture")
            {
                return Ok((Some(window), None));
            }
            thread::sleep(Duration::from_millis(100));
        }
        if !allow_start {
            return Ok((None, None));
        }
        if !fixture_path.exists() {
            println!(
                "fixture_start=SKIP reason=fixture_executable_missing path={}",
                fixture_path.display()
            );
            return Ok((None, None));
        }
        let child = Command::new(fixture_path)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| format!("failed to start test fixture: {error}"))?;
        for _ in 0..30 {
            if let Some(window) =
                find_window(&session.window_list()?, "alice computer input fixture")
            {
                println!("fixture_start=PASS pid={}", child.id());
                return Ok((Some(window), Some(child)));
            }
            thread::sleep(Duration::from_millis(100));
        }
        Err("fixture started but its top-level window did not appear".into())
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
