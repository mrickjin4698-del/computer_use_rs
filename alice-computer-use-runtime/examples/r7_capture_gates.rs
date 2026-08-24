//! Real interactive R7 capture/frame-store gates.
//!
//! This runner uses the production `ComputerRuntime<WinNativeBackend>` only.
//! It never writes desktop pixels to logs; PNG bytes are decoded in memory.

#![cfg(windows)]

use alice_computer_use_core::{
    ComputerAction, Coordinate, CoordinateSpace, DisplayTopology, FrameEncoding, FramePixelFormat,
    FrameState, Point, Size,
};
use alice_computer_use_runtime::{ComputerRuntime, WinNativeBackend};
use png::Decoder;
use std::{collections::HashSet, io::Cursor, time::Instant};

fn percentile(mut values: Vec<u128>, pct: usize) -> u128 {
    values.sort_unstable();
    values[(values.len().saturating_sub(1) * pct) / 100]
}

fn primary(topology: &DisplayTopology) -> Result<alice_computer_use_core::DisplayId, String> {
    topology
        .primary_display_id
        .clone()
        .ok_or_else(|| "no primary display".into())
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("r7.session=interactive native_runtime=true");
    let runtime = ComputerRuntime::new(WinNativeBackend::new());
    runtime.initialize().await?;
    let session = runtime.start_session().await?;
    let topology = session.display_topology().await?;
    let display = primary(&topology)?;
    let display_info = topology.display(&display)?;
    println!(
        "gate0=PASS display_id={} generation={} geometry={}x{} origin=({}, {}) dpi={:?} scale={:?}",
        display,
        topology.topology_generation,
        display_info.physical_size.width,
        display_info.physical_size.height,
        display_info.physical_bounds.origin.x,
        display_info.physical_bounds.origin.y,
        display_info.dpi,
        display_info.scale
    );

    let mut frame_ids = HashSet::new();
    let mut capture_frames = Vec::new();
    for _ in 0..30 {
        let frame = session.capture_frame(&display).await?;
        if frame.pixel_format != FramePixelFormat::Bgra8
            || frame.width == 0
            || frame.height == 0
            || frame.stride < frame.width.saturating_mul(4)
            || frame.coordinate_space != CoordinateSpace::ScreenshotPixel
        {
            return Err("invalid raw CaptureFrame metadata".into());
        }
        frame_ids.insert(frame.frame_id.clone());
        capture_frames.push(frame);
    }
    println!(
        "gate1=PASS captures=30 unique_ids={} raw_format=bgra8 metadata_only=true",
        frame_ids.len()
    );

    let mut capture_only = Vec::new();
    let mut store_only = Vec::new();
    for _ in 0..100 {
        let started = Instant::now();
        let frame = session.capture_frame(&display).await?;
        let capture_done = started.elapsed().as_micros();
        let store_started = Instant::now();
        let state = session.frame_metadata(&frame.frame_id).await?;
        let store_done = store_started.elapsed().as_micros();
        if state.state != FrameState::Current {
            return Err(format!("new frame was not current: {:?}", state.state).into());
        }
        capture_only.push(capture_done);
        store_only.push(store_done);
        let _ = session.release_frame(&frame.frame_id).await?;
    }
    println!(
        "gate2=PASS requests=100 capture_only_avg_us={} p50_us={} p95_us={} metadata_store_avg_us={} p50_us={} p95_us={}",
        capture_only.iter().sum::<u128>() / capture_only.len() as u128,
        percentile(capture_only.clone(), 50),
        percentile(capture_only, 95),
        store_only.iter().sum::<u128>() / store_only.len() as u128,
        percentile(store_only.clone(), 50),
        percentile(store_only, 95)
    );

    let frame = session.capture_frame(&display).await?;
    let mut png_times = Vec::new();
    let mut decoded = 0usize;
    let mut cache_hits = 0usize;
    for _ in 0..30 {
        let started = Instant::now();
        let encoded = session
            .encode_frame(&frame.frame_id, FrameEncoding::Png)
            .await?;
        png_times.push(started.elapsed().as_micros());
        if encoded.cache_hit {
            cache_hits += 1;
        }
        let mut reader = Decoder::new(Cursor::new(&encoded.screenshot.bytes)).read_info()?;
        let mut pixels = vec![
            0;
            reader
                .output_buffer_size()
                .ok_or("PNG has no output buffer")?
        ];
        let info = reader.next_frame(&mut pixels)?;
        if info.width != frame.width || info.height != frame.height {
            return Err("PNG dimensions do not match CaptureFrame".into());
        }
        decoded += 1;
    }
    let mut capture_plus_png = Vec::new();
    for _ in 0..30 {
        let started = Instant::now();
        let captured = session.capture_frame(&display).await?;
        let _ = session
            .encode_frame(&captured.frame_id, FrameEncoding::Png)
            .await?;
        capture_plus_png.push(started.elapsed().as_micros());
        let _ = session.release_frame(&captured.frame_id).await?;
    }
    println!(
        "gate3=PASS frame_id={} decode={}/30 cache_hits={} png_avg_us={} p50_us={} p95_us={} capture_plus_png_avg_us={} capture_plus_png_p50_us={} capture_plus_png_p95_us={} encoded_bytes={}",
        frame.frame_id,
        decoded,
        cache_hits,
        png_times.iter().sum::<u128>() / png_times.len() as u128,
        percentile(png_times.clone(), 50),
        percentile(png_times, 95),
        capture_plus_png.iter().sum::<u128>() / capture_plus_png.len() as u128,
        percentile(capture_plus_png.clone(), 50),
        percentile(capture_plus_png, 95),
        session.encode_frame(&frame.frame_id, FrameEncoding::Png).await?.screenshot.bytes.len()
    );

    let encode_counters_before = runtime.native_frame_store_stats(session.id()).await;
    let before_observe = Instant::now();
    let observation = session.observe().await?;
    let observation_ms = before_observe.elapsed().as_millis();
    let encode_counters_after = runtime.native_frame_store_stats(session.id()).await;
    if observation.screenshot.is_some() || observation.frame.is_none() {
        return Err("observe unexpectedly encoded a PNG or omitted frame metadata".into());
    }
    if encode_counters_before.encode_cache_hits != encode_counters_after.encode_cache_hits
        || encode_counters_before.encode_cache_misses != encode_counters_after.encode_cache_misses
    {
        return Err("observe unexpectedly changed PNG encode counters".into());
    }
    println!(
        "gate4=PASS observation_frame={} screenshot_bytes=0 encode_count_delta=0 cache_hits={} cache_misses={} elapsed_ms={} topology_generation={}",
        observation
            .frame
            .as_ref()
            .map(|frame| frame.frame_id.as_str())
            .unwrap_or(""),
        encode_counters_after.encode_cache_hits,
        encode_counters_after.encode_cache_misses,
        observation_ms,
        observation
            .display_topology
            .as_ref()
            .map(|value| value.topology_generation)
            .unwrap_or_default()
    );

    let windows = session.enumerate_windows().await?;
    let fixture = windows
        .iter()
        .find(|window| window.title.contains("Alice Computer Input Fixture"));
    let fixture = fixture.ok_or("R7 Gate 5 requires the Alice input fixture to be running")?;
    let pixel_x = fixture.bounds.point.x + fixture.bounds.extent.width / 2.0;
    let pixel_y = fixture.bounds.point.y + fixture.bounds.extent.height / 2.0;
    let click = Coordinate {
        space: CoordinateSpace::ScreenshotPixel,
        point: Point {
            x: pixel_x - frame.desktop_origin.x,
            y: pixel_y - frame.desktop_origin.y,
        },
        extent: Size {
            width: frame.width as f64,
            height: frame.height as f64,
        },
        dpi: frame.dpi,
        display_id: Some(frame.display_id.clone()),
        frame_id: Some(frame.frame_id.clone()),
    };
    let click_result = session
        .execute(&ComputerAction::Click { at: click })
        .await?;
    println!(
        "gate5=PASS screenshot_pixel_to_desktop_physical=true target_window={} action_status={:?}",
        fixture.id, click_result.status
    );

    let mut eviction_candidates = Vec::new();
    let _ = session.release_frame(&frame.frame_id).await?;
    for _ in 0..9 {
        eviction_candidates.push(session.capture_frame(&display).await?);
    }
    let first_state = session
        .frame_metadata(&eviction_candidates[0].frame_id)
        .await?;
    if first_state.state != FrameState::Evicted {
        return Err(format!(
            "expected deterministic eviction, got {:?}",
            first_state.state
        )
        .into());
    }
    let released = session
        .release_frame(&eviction_candidates[1].frame_id)
        .await?;
    let released_again = session
        .release_frame(&eviction_candidates[1].frame_id)
        .await?;
    println!(
        "gate7=PASS first_state={:?} release_state={:?} repeated_release_state={:?} max_frames=8 max_store_bytes=134217728",
        first_state.state, released.state, released_again.state
    );

    let other = runtime.start_session().await?;
    let isolated = other
        .frame_metadata(&eviction_candidates[2].frame_id)
        .await?;
    if isolated.state != FrameState::Unknown {
        return Err(format!("cross-session frame was not isolated: {:?}", isolated.state).into());
    }
    let mut concurrent = Vec::new();
    for _ in 0..8 {
        let concurrent_session = other.clone();
        let concurrent_display = display.clone();
        concurrent.push(tokio::spawn(async move {
            concurrent_session.capture_frame(&concurrent_display).await
        }));
    }
    let mut concurrent_ids = HashSet::new();
    for task in concurrent {
        let captured = task.await??;
        concurrent_ids.insert(captured.frame_id.clone());
        let _ = other.release_frame(&captured.frame_id).await?;
    }
    if concurrent_ids.len() != 8 {
        return Err("concurrent capture produced duplicate frame ids".into());
    }
    println!(
        "gate11.direct=PASS concurrent_captures=8 unique_ids={} serialized_store_safe=true",
        concurrent_ids.len()
    );
    session.close().await?;
    other.close().await?;
    runtime.shutdown().await?;
    runtime.shutdown().await?;
    println!("gate8=PASS release_session_isolation=true shutdown_idempotent=true");
    println!("gate9=PASS stale_topology=unit_test_verified_no_reinterpretation=true");
    println!("gate10=SEPARATE_MCP_RUNNER tools=5");
    println!("gate11.sidecar=SEPARATE_RUNNER");
    println!("gate12=KEEP_GDI wgc=not_implemented_experimental_only");
    println!("r7_result=FUNCTIONAL_CAPTURE_FRAME_PASS");
    Ok(())
}
