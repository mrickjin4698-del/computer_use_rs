//! Host-side process/RPC smoke runner for Computer-INFRA-R0.5.
//!
//! This intentionally exercises lifecycle and observation transport only. It
//! does not implement a second backend or issue an input action.

use alice_computer_use_sidecar_client::{ComputerSidecarClient, SidecarError};
use std::{env, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let executable = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("target/debug/alice-computer.exe"));
    let timeout = Duration::from_secs(10);

    let client = ComputerSidecarClient::spawn(&executable, timeout)?;
    let hello = client.hello()?;
    let health = client.health()?;
    println!(
        "spawned pid={} protocol={} backend={} capabilities={} ready={}",
        client.process_id(),
        hello.protocol_version,
        hello.backend,
        hello.capabilities.len(),
        health.ready
    );

    let session = client.create_session()?;
    println!("session.create id={}", session.id());
    match session.window_list() {
        Ok(windows) => println!("window.list count={}", windows.len()),
        Err(error) => println!("window.list error={} code={}", error, error.code()),
    }

    session.close()?;
    match session.window_list() {
        Err(SidecarError::SessionInvalidated { .. }) => {
            println!("closed-session rejection=PASS")
        }
        Err(error) => println!("closed-session rejection={} code={}", error, error.code()),
        Ok(_) => println!("closed-session rejection=FAIL"),
    }

    client.shutdown()?;
    client.shutdown()?;
    println!("shutdown idempotent alive={}", client.is_alive());

    let restarted = ComputerSidecarClient::spawn(&executable, timeout)?;
    let restarted_session = restarted.create_session()?;
    println!(
        "reinitialize pid={} session={}",
        restarted.process_id(),
        restarted_session.id()
    );
    restarted_session.close()?;
    restarted.shutdown()?;
    println!("reinitialize shutdown alive={}", restarted.is_alive());
    Ok(())
}
