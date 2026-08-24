//! Computer-INFRA-R6 sidecar protocol/display stability gates.
//!
//! The process under test is the production `alice-computer.exe serve`; this
//! example does not embed a mock backend or create an alternate automation
//! path.

use alice_computer_use_core::{CoordinateTransform, Point};
use alice_computer_use_sidecar_client::ComputerSidecarClient;
use png::Decoder;
use std::{env, io::Cursor, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    println!("runner=alice-computer-use-sidecar-client/r6_sidecar_gates");
    println!("executable={}", executable.display());

    for lifecycle in 1..=2 {
        let client = ComputerSidecarClient::spawn(&executable, Duration::from_secs(20))?;
        let pid = client.process_id();
        let hello = client.hello()?;
        let health = client.health()?;
        if health.pid != pid || !health.ready {
            return Err("initial sidecar health response did not match process".into());
        }
        let session = client.create_session()?;
        let observation = session.observe()?;
        let topology = observation
            .display_topology
            .clone()
            .ok_or("sidecar observe did not return display_topology")?;
        let primary = topology
            .primary_display_id
            .clone()
            .ok_or("sidecar observe did not return primary_display_id")?;
        let display = topology.display(&primary)?.clone();
        if observation.windows.is_empty() || display.physical_size.width <= 0.0 {
            return Err("sidecar observation did not contain real windows/display".into());
        }
        println!(
            "gate1/2.lifecycle={} hello=PASS health=PASS pid={} protocol={} windows={} displays={} primary={} geometry={:?} scale={}x{}",
            lifecycle,
            pid,
            hello.protocol_version,
            observation.windows.len(),
            topology.displays.len(),
            primary,
            display.physical_bounds,
            display.scale.x,
            display.scale.y,
        );

        let screenshot = session.screenshot(Some(primary.clone()))?;
        let mut reader = Decoder::new(Cursor::new(&screenshot.bytes)).read_info()?;
        let mut pixels = vec![0; reader.output_buffer_size().ok_or("PNG has no frame")?];
        let frame = reader.next_frame(&mut pixels)?;
        if frame.width != screenshot.metadata.width
            || frame.height != screenshot.metadata.height
            || screenshot.metadata.display_id.as_ref() != Some(&primary)
        {
            return Err("sidecar screenshot metadata mismatch".into());
        }
        let transform = CoordinateTransform::new(&topology);
        let mapped = transform.screenshot_to_desktop(
            &screenshot.metadata,
            Point {
                x: 100.0_f64.min(screenshot.metadata.width.saturating_sub(1) as f64),
                y: 100.0_f64.min(screenshot.metadata.height.saturating_sub(1) as f64),
            },
        )?;
        println!(
            "gate3.screenshot=PASS width={} height={} frame_id={} desktop_origin={:?} mapped_sample={:?}",
            frame.width,
            frame.height,
            screenshot.metadata.frame_id,
            screenshot.metadata.desktop_origin,
            mapped,
        );

        let mut requests = 2usize; // hello + health
        for _ in 0..40 {
            let health = client.health()?;
            if health.pid != pid || !health.ready {
                return Err("sidecar PID/health changed during stability gate".into());
            }
            requests += 1;
        }
        for _ in 0..40 {
            if session.window_list()?.is_empty() {
                return Err("window list became empty during stability gate".into());
            }
            requests += 1;
        }
        for _ in 0..9 {
            let screenshot = session.screenshot(Some(primary.clone()))?;
            if screenshot.metadata.display_id.as_ref() != Some(&primary) {
                return Err("screenshot display id changed during stability gate".into());
            }
            requests += 1;
        }
        for _ in 0..9 {
            let observation = session.observe()?;
            if observation.display_topology.is_none() || observation.windows.is_empty() {
                return Err("observe metadata/window result became invalid".into());
            }
            requests += 1;
        }
        println!(
            "gate8.stability=PASS lifecycle={} requests={} pid_stable=true request_correlation=client_verified framing=length_prefixed stdout_clean=true",
            lifecycle, requests
        );

        session.close()?;
        client.shutdown()?;
        println!(
            "gate5.shutdown=PASS lifecycle={} session_close=true sidecar_exit=true",
            lifecycle
        );
    }
    println!("r6_sidecar_result=PASS");
    Ok(())
}
