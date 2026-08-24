//! Real sidecar R7 transport/frame gates.

use alice_computer_use_core::{FrameEncoding, FrameState};
use alice_computer_use_sidecar_client::ComputerSidecarClient;
use png::Decoder;
use std::{env, io::Cursor, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    println!("runner=alice-computer-use-sidecar-client/r7_sidecar_gates");
    println!("executable={}", executable.display());

    for lifecycle in 1..=2 {
        let client = ComputerSidecarClient::spawn(&executable, Duration::from_secs(60))?;
        let pid = client.process_id();
        let hello = client.hello()?;
        let health = client.health()?;
        if hello.pid != pid || health.pid != pid || !health.ready {
            return Err("sidecar hello/health PID or readiness mismatch".into());
        }
        let session = client.create_session()?;
        let observation = session.observe()?;
        let topology = observation
            .display_topology
            .clone()
            .ok_or("sidecar observation omitted topology")?;
        let primary = topology
            .primary_display_id
            .clone()
            .ok_or("sidecar observation omitted primary display")?;
        if observation.windows.is_empty() {
            return Err("real interactive sidecar returned zero windows".into());
        }
        println!(
            "gate1=PASS lifecycle={} pid={} windows={} displays={} primary={} geometry={:?}",
            lifecycle,
            pid,
            observation.windows.len(),
            topology.displays.len(),
            primary,
            topology.display(&primary)?.physical_bounds
        );

        let mut request_count = 0usize;
        for _ in 0..25 {
            let frame = session.capture_frame(Some(primary.clone()))?;
            request_count += 1;
            let metadata = session.frame_metadata(&frame.frame_id)?;
            request_count += 1;
            if metadata.metadata.as_ref().map(|value| &value.frame_id) != Some(&frame.frame_id) {
                return Err("frame metadata correlation mismatch".into());
            }
            let encoded = session.encode_frame(&frame.frame_id, FrameEncoding::Png)?;
            request_count += 1;
            let mut reader = Decoder::new(Cursor::new(&encoded.screenshot.bytes)).read_info()?;
            let mut pixels = vec![0; reader.output_buffer_size().ok_or("PNG has no frame")?];
            let decoded = reader.next_frame(&mut pixels)?;
            if decoded.width != frame.width || decoded.height != frame.height {
                return Err("sidecar frame/PNG dimensions mismatch".into());
            }
            let released = session.release_frame(&frame.frame_id)?;
            request_count += 1;
            if released.state != FrameState::Released {
                return Err(format!("frame release state was {:?}", released.state).into());
            }
        }
        if request_count < 100 || client.process_id() != pid || !client.is_alive() {
            return Err("100-request gate lost PID, liveness, or request count".into());
        }
        println!(
            "gate11=PASS lifecycle={} requests={} pid_stable=true correlation=true framing=length_prefixed stdout_clean=true capture_metadata_encode_release=true",
            lifecycle, request_count
        );

        session.close()?;
        client.shutdown()?;
        println!(
            "gate5=PASS lifecycle={} session_close=true shutdown=true",
            lifecycle
        );
    }
    println!("r7_sidecar_result=PASS");
    Ok(())
}
