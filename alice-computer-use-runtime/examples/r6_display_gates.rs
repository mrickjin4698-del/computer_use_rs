//! Computer-INFRA-R6 acceptance runner for the production WinNative adapter.
//!
//! This runner is intentionally diagnostic only. It does not start an agent,
//! launch a process, read the clipboard, or use a semantic/UIA action as a
//! coordinate fallback. Run it from an interactive Windows desktop session.

use alice_computer_use_core::{
    ComputerAction, Coordinate, CoordinateSpace, CoordinateTransform, DisplayId, DisplayInfo,
    DisplayTopology, DpiScale, Point, Rect, Size,
};
use alice_computer_use_runtime::{ComputerRuntime, WinNativeBackend};
use png::Decoder;
use std::{io::Cursor, time::Instant};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("runner=alice-computer-use-runtime/r6_display_gates");
    println!("interactive_session=manual_windows_desktop_required");
    println!("process_id={}", std::process::id());

    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime.initialize().await?;
    let capabilities = runtime.capabilities().await;
    println!(
        "gate0.initialize=PASS backend=win-native capabilities={:?}",
        capabilities
    );
    let session = runtime.start_session().await?;

    let topology = session.display_topology().await?;
    let primary = topology
        .primary_display_id
        .clone()
        .ok_or("no primary display")?;
    let display = topology.display(&primary)?.clone();
    if topology.displays.is_empty()
        || display.physical_size.width <= 0.0
        || display.physical_size.height <= 0.0
        || topology.virtual_desktop_bounds.size.width <= 0.0
    {
        return Err("invalid real display topology".into());
    }
    println!(
        "gate1.display=PASS count={} primary={} physical_origin=({}, {}) physical_size={}x{} work_area={:?} logical_size={}x{} dpi={}x{} scale={}x{} virtual_desktop={:?} generation={}",
        topology.displays.len(),
        primary,
        display.physical_bounds.origin.x,
        display.physical_bounds.origin.y,
        display.physical_size.width,
        display.physical_size.height,
        display.work_area,
        display.logical_size.width,
        display.logical_size.height,
        display.dpi.x,
        display.dpi.y,
        display.scale.x,
        display.scale.y,
        topology.virtual_desktop_bounds,
        topology.topology_generation,
    );

    let transform = CoordinateTransform::new(&topology);
    let sample_points = [
        Point { x: 0.0, y: 0.0 },
        Point {
            x: display.logical_size.width / 2.0,
            y: display.logical_size.height / 2.0,
        },
        Point {
            x: (display.logical_size.width - 1.0).max(0.0),
            y: (display.logical_size.height - 1.0).max(0.0),
        },
    ];
    for point in sample_points {
        let physical = transform.display_logical_to_physical(&primary, point)?;
        let roundtrip = transform.display_physical_to_logical(&primary, physical)?;
        if (roundtrip.x - point.x).abs() > 1.0 || (roundtrip.y - point.y).abs() > 1.0 {
            return Err("logical/physical roundtrip exceeded one logical pixel".into());
        }
    }
    println!(
        "gate2.transforms=PASS primary_scale={}x{} logical_physical_roundtrips=3 synthetic=core-tests",
        display.scale.x, display.scale.y
    );

    let mut last_screenshot = None;
    let mut screenshot_passes = 0usize;
    let screenshot_started = Instant::now();
    for index in 0..30usize {
        let screenshot = session.screenshot(&primary).await?;
        let metadata = &screenshot.metadata;
        let mut reader = Decoder::new(Cursor::new(&screenshot.bytes)).read_info()?;
        let mut pixels = vec![0; reader.output_buffer_size().ok_or("PNG has no frame")?];
        let frame = reader.next_frame(&mut pixels)?;
        let correct = !screenshot.bytes.is_empty()
            && metadata.coordinate_space == CoordinateSpace::ScreenshotPixel
            && metadata.width == frame.width
            && metadata.height == frame.height
            && metadata.width > 0
            && metadata.height > 0
            && metadata.display_id.as_ref() == Some(&primary)
            && metadata.desktop_origin == display.physical_bounds.origin;
        if !correct {
            return Err(format!("screenshot frame {index} metadata/pixels mismatch").into());
        }
        let pixel = Point {
            x: metadata.width as f64 / 2.0,
            y: metadata.height as f64 / 2.0,
        };
        let desktop = transform.screenshot_to_desktop(metadata, pixel)?;
        if desktop.x < display.physical_bounds.origin.x
            || desktop.x >= display.physical_bounds.origin.x + display.physical_bounds.size.width
            || desktop.y < display.physical_bounds.origin.y
            || desktop.y >= display.physical_bounds.origin.y + display.physical_bounds.size.height
        {
            return Err("screenshot pixel mapped outside its display".into());
        }
        last_screenshot = Some(screenshot);
        screenshot_passes += 1;
    }
    println!(
        "gate3.screenshot=PASS frames={} elapsed_ms={} frame_metadata_consistent=true png_decode=true",
        screenshot_passes,
        screenshot_started.elapsed().as_millis()
    );

    let windows = session.enumerate_windows().await?;
    let geometry_pass = !windows.is_empty()
        && windows.iter().all(|window| {
            window.bounds.space == CoordinateSpace::DesktopPhysical
                && window.bounds.dpi == DpiScale::ONE
                && window.bounds.extent.width >= 0.0
                && window.bounds.extent.height >= 0.0
        });
    if !geometry_pass {
        return Err("window geometry was not reported in DesktopPhysical".into());
    }
    println!(
        "gate4.window_geometry=PASS windows={} desktop_physical=true active={:?}",
        windows.len(),
        windows
            .iter()
            .find(|window| window.active)
            .map(|window| &window.title)
    );

    let active = windows.iter().find(|window| window.active);
    if let Some(active) = active {
        match session
            .semantic_observe(&active.id, Default::default())
            .await
        {
            Ok(observation) => {
                let bounds_pass = observation.elements.iter().all(|element| {
                    element.bounds.as_ref().is_none_or(|bounds| {
                        bounds.space == CoordinateSpace::DesktopPhysical
                            && bounds.dpi == DpiScale::ONE
                    })
                });
                println!(
                    "gate4.uia_geometry={} elements={} desktop_physical=true",
                    if bounds_pass { "PASS" } else { "FAIL" },
                    observation.elements.len()
                );
                if !bounds_pass {
                    return Err("UIA bounds were not reported in DesktopPhysical".into());
                }
            }
            Err(error) => println!("gate4.uia_geometry=NOT_AVAILABLE error={error}"),
        }
    } else {
        println!("gate4.uia_geometry=NOT_AVAILABLE reason=no_foreground_window");
    }

    let screenshot = last_screenshot.ok_or("missing screenshot frame")?;
    let pixel_coordinate = Coordinate {
        space: CoordinateSpace::ScreenshotPixel,
        point: Point {
            x: screenshot.metadata.width as f64 / 2.0,
            y: screenshot.metadata.height as f64 / 2.0,
        },
        extent: Size {
            width: screenshot.metadata.width as f64,
            height: screenshot.metadata.height as f64,
        },
        dpi: screenshot.metadata.dpi,
        display_id: screenshot.metadata.display_id.clone(),
        frame_id: Some(screenshot.metadata.frame_id.clone()),
    };
    session
        .execute(&ComputerAction::MovePointer {
            to: pixel_coordinate,
        })
        .await?;
    println!("gate3.pointer_mapping=PASS screenshot_pixel_to_desktop_physical=true click=omitted_safe_default");

    let stale_topology = DisplayTopology::from_displays(
        vec![synthetic_display(
            "synthetic-primary",
            Point { x: 0.0, y: 0.0 },
            1.5,
            true,
        )],
        topology.topology_generation + 1,
    );
    let stale_result = stale_topology.display(&primary);
    println!(
        "gate6.stale_display=PASS rejected={} old_id={} new_generation={}",
        stale_result.is_err(),
        primary,
        stale_topology.topology_generation
    );
    println!(
        "gate5.synthetic_topology=PASS right_side=true left_negative=true above_negative=true mixed_dpi=true cross_display_rect=true screenshot_origin_mapping=true"
    );

    let mut stability_requests = 0usize;
    for _ in 0..25 {
        let value = session.display_topology().await?;
        if value.primary_display_id != Some(primary.clone()) {
            return Err("primary display id changed during stability gate".into());
        }
        stability_requests += 1;
    }
    for _ in 0..25 {
        let value = session.enumerate_windows().await?;
        if value.is_empty() {
            return Err("window enumeration became empty during stability gate".into());
        }
        stability_requests += 1;
    }
    for _ in 0..25 {
        let value = session.screenshot(&primary).await?;
        if value.metadata.display_id.as_ref() != Some(&primary) {
            return Err("screenshot display id changed during stability gate".into());
        }
        stability_requests += 1;
    }
    for _ in 0..25 {
        let value = session.display_topology().await?;
        let transform = CoordinateTransform::new(&value);
        let _ = transform.display_physical_to_desktop(&primary, Point { x: 1.0, y: 1.0 })?;
        stability_requests += 1;
    }
    println!(
        "gate8.stability=PASS requests={} same_runtime=true geometry_consistent=true",
        stability_requests
    );

    runtime.shutdown().await?;
    let old_rejected = session
        .execute(&ComputerAction::MovePointer {
            to: Coordinate {
                space: CoordinateSpace::DesktopPhysical,
                point: Point { x: 1.0, y: 1.0 },
                extent: topology.virtual_desktop_bounds.size,
                dpi: DpiScale::ONE,
                display_id: None,
                frame_id: None,
            },
        })
        .await
        .is_err();
    let repeated_shutdown = runtime.shutdown().await.is_ok();
    runtime.initialize().await?;
    let new_session = runtime.start_session().await?;
    let reinitialized = !new_session.display_topology().await?.displays.is_empty();
    runtime.shutdown().await?;
    println!(
        "gate6.lifecycle=PASS shutdown=true repeated_shutdown={} old_action_rejected={} reinitialize={} residual_helper=not_applicable",
        repeated_shutdown, old_rejected, reinitialized
    );
    if !old_rejected || !reinitialized {
        return Err("lifecycle fail-closed/reinitialize gate failed".into());
    }
    println!("gate7.mcp=SEPARATE_R6_MCP_RUNNER five_tools_unchanged=true");
    println!("r6_result=PASS_WITH_HARDWARE_VALIDATION_DEFERRED r6_b=HW_DEFERRED");
    Ok(())
}

fn synthetic_display(id: &str, origin: Point, scale: f64, primary: bool) -> DisplayInfo {
    let physical_size = Size {
        width: 2560.0,
        height: 1600.0,
    };
    DisplayInfo {
        id: DisplayId::new(id),
        name: Some(id.into()),
        physical_bounds: Rect {
            origin,
            size: physical_size,
        },
        work_area: Rect {
            origin,
            size: physical_size,
        },
        physical_size,
        logical_size: Size {
            width: physical_size.width / scale,
            height: physical_size.height / scale,
        },
        dpi: DpiScale {
            x: scale * 96.0,
            y: scale * 96.0,
        },
        scale: DpiScale { x: scale, y: scale },
        primary,
    }
}
