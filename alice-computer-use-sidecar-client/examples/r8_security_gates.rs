//! Manual interactive R8 security-boundary gates.
//!
//! The fixture must be started by the user.  Running it normally exercises
//! the allowed path; running the same executable manually as administrator and
//! rerunning this example exercises the fail-closed elevated-target path.

#![cfg(windows)]

use alice_computer_use_core::{
    ActionStatus, ComputerAction, ComputerExecutionOutcome, ComputerExecutionRequest, Coordinate,
    CoordinateSpace, DpiScale, Point, SemanticAction, SemanticObservationLimits,
};
use alice_computer_use_sidecar_client::{ComputerSidecarClient, SidecarError};
use std::{env, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sidecar = PathBuf::from("target/release/alice-computer.exe");
    let mut title = "Alice Computer R8 Integrity Fixture".to_owned();
    let mut exercise = false;
    let mut arguments = env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--sidecar" => {
                sidecar = PathBuf::from(arguments.next().ok_or("--sidecar needs a path")?)
            }
            "--target-title" => title = arguments.next().ok_or("--target-title needs text")?,
            "--exercise" => exercise = true,
            "--help" | "-h" => {
                println!(
                    "usage: r8_security_gates [--sidecar path] [--target-title text] [--exercise]"
                );
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    println!("runner=alice-computer-use-sidecar-client/r8_security_gates");
    println!("sidecar={}", sidecar.display());
    println!("target_title={title}");
    let client = ComputerSidecarClient::spawn(&sidecar, Duration::from_secs(60))?;
    let hello = client.hello()?;
    let health = client.health()?;
    let security = health
        .security
        .clone()
        .ok_or("health omitted sidecar security context")?;
    let desktop = health
        .desktop
        .clone()
        .ok_or("health omitted desktop security context")?;
    println!(
        "gate1=PASS pid={} hello_pid={} integrity={:?} elevated={} ui_access={} app_container={} desktop={:?} interactive={} protected={}",
        client.process_id(),
        hello.pid,
        security.integrity_level,
        security.elevated,
        security.ui_access,
        security.app_container,
        desktop.desktop_kind,
        desktop.interactive,
        desktop.protected,
    );

    let session = client.create_session()?;
    let observation = session.observe()?;
    if observation.windows.is_empty() {
        return Err("interactive observation returned zero windows".into());
    }
    let frame = observation
        .frame
        .as_ref()
        .ok_or("observe omitted frame metadata")?;
    println!(
        "gate2=PASS windows={} screens={} frame={} {}x{} dpi={:?} scale={:?}",
        observation.windows.len(),
        observation.screens.len(),
        frame.frame_id,
        frame.width,
        frame.height,
        frame.dpi,
        frame.scale,
    );

    for request_index in 0..100usize {
        if request_index % 2 == 0 {
            if session.window_list()?.is_empty() {
                return Err("window.list became empty during R8 stability loop".into());
            }
        } else if session.observe()?.windows.is_empty() {
            return Err("observe became empty during R8 stability loop".into());
        }
    }
    println!(
        "gate12=PASS requests=100 pid_stable={} security_inspection=true observation=true framing=true",
        client.process_id() == hello.pid
    );

    let target = observation
        .windows
        .iter()
        .find(|window| window.title.contains(&title))
        .cloned();
    let Some(target) = target else {
        println!("gate3=DEFERRED target_window_not_found=true");
        session.close()?;
        client.shutdown()?;
        return Ok(());
    };
    let target_security = target
        .security
        .clone()
        .ok_or("target omitted security metadata")?;
    println!(
        "target=id={} title={:?} pid={:?} boundary={:?} process={:?} capabilities={:?}",
        target.id,
        target.title,
        target.process_id,
        target_security.boundary,
        target_security.process,
        target_security.capabilities,
    );

    let coordinate = Coordinate {
        space: CoordinateSpace::DesktopPhysical,
        point: Point {
            x: target.bounds.point.x + 100.0,
            y: target.bounds.point.y + 60.0,
        },
        extent: observation
            .screens
            .iter()
            .find(|screen| screen.primary)
            .map(|screen| screen.physical_size)
            .unwrap_or(target.bounds.extent),
        dpi: DpiScale::ONE,
        display_id: target.screen_id.clone(),
        frame_id: None,
    };

    if target_security.boundary == alice_computer_use_core::ComputerAccessBoundary::Allowed {
        if !exercise {
            println!("gate3=READY normal_target=true exercise=false");
        } else {
            session.action(&ComputerAction::FocusWindow {
                window_id: target.id.clone(),
            })?;
            let click = session.action(&ComputerAction::Click { at: coordinate })?;
            if click.status != ActionStatus::Performed {
                return Err("normal target click was not performed".into());
            }
            let unique = format!("R8_NORMAL_{}", std::process::id());
            session.action(&ComputerAction::TypeText {
                text: unique.clone(),
                target: Some(target.id.clone()),
                at: None,
            })?;
            let after = session.window_list()?;
            let verified = after
                .iter()
                .find(|window| window.id == target.id)
                .map(|window| window.title.contains(&unique))
                .unwrap_or(false);
            if !verified {
                return Err("normal fixture title did not verify the typed text".into());
            }
            println!("gate3=PASS normal_same_integrity=true click=true type_text_verified=true");
        }
    } else if exercise {
        let input_error = session
            .action(&ComputerAction::TypeText {
                text: "R8_MUST_NOT_BE_TYPED".into(),
                target: Some(target.id.clone()),
                at: None,
            })
            .expect_err("elevated target input was unexpectedly accepted");
        if !matches!(input_error, SidecarError::Rpc(ref error) if error.code == "ELEVATION_REQUIRED" || error.code == "PROTECTED_DESKTOP")
        {
            return Err(format!("unexpected elevated input error: {input_error}").into());
        }
        let result = session.execute_policy(&ComputerExecutionRequest::pixel(
            ComputerAction::Click { at: coordinate },
            Some(target.id.clone()),
        ))?;
        if !matches!(
            result.final_outcome,
            ComputerExecutionOutcome::ElevationRequired
                | ComputerExecutionOutcome::ProtectedDesktop
                | ComputerExecutionOutcome::SecurityContextUnavailable
        ) {
            return Err(format!(
                "elevated pixel action was not denied: {:?}",
                result.final_outcome
            )
            .into());
        }
        println!(
            "gate3=PASS elevated_target=true boundary={:?} input_error={} pixel_outcome={:?} send_input=0 pixel_fallback=0",
            target_security.boundary,
            input_error.code(),
            result.final_outcome,
        );

        if let Ok(semantic) =
            session.semantic_observe(&target.id, SemanticObservationLimits::default())
        {
            if let Some(element) = semantic.elements.first() {
                let error = session
                    .semantic_action(&SemanticAction::Focus {
                        element_id: element.id.clone(),
                    })
                    .expect_err("elevated semantic action was unexpectedly accepted");
                println!(
                    "gate4=PASS semantic_denied={} element_id_present=true",
                    error.code()
                );
            } else {
                println!("gate4=DEFERRED semantic_elements=0");
            }
        } else {
            println!("gate4=DEFERRED semantic_observation_unavailable=true");
        }
    } else {
        println!(
            "gate3=READY target_boundary={:?} exercise=false",
            target_security.boundary
        );
    }

    session.close()?;
    client.shutdown()?;
    println!("gate5=PASS session_close=true shutdown=true");
    Ok(())
}
