use alice_computer_use_sidecar_client::{
    ComputerHostService, ComputerHostServiceConfig, ComputerHostServiceState,
};
use std::{env, path::PathBuf, time::Duration};

#[cfg(windows)]
fn force_terminate(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows_sys::Win32::System::Threading::{
        OpenProcess, TerminateProcess, WaitForSingleObject, PROCESS_TERMINATE,
    };

    const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;
    let handle = unsafe { OpenProcess(PROCESS_TERMINATE | SYNCHRONIZE_ACCESS, 0, pid) };
    if handle.is_null() {
        return Err(format!("OpenProcess failed for pid {pid}").into());
    }
    let terminated = unsafe { TerminateProcess(handle, 1) } != 0;
    let exited = terminated && unsafe { WaitForSingleObject(handle, 5_000) == WAIT_OBJECT_0 };
    unsafe { CloseHandle(handle) };
    if terminated && exited {
        Ok(())
    } else {
        Err(format!("could not terminate pid {pid}").into())
    }
}

#[cfg(not(windows))]
fn force_terminate(_pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    Err("crash gate requires Windows".into())
}

fn sidecar_path() -> PathBuf {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sidecar" {
            return PathBuf::from(args.next().expect("--sidecar requires a path"));
        }
    }
    PathBuf::from("target/release/alice-computer.exe")
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = sidecar_path();
    let service = ComputerHostService::new(ComputerHostServiceConfig::new(
        path.clone(),
        Duration::from_secs(5),
    ));
    println!("sidecar_path={}", path.display());

    let first = service.ensure_started()?;
    println!(
        "started state={:?} pid={:?} version={:?} protocol={:?} generation={}",
        first.state,
        first.process_id,
        first.sidecar_version,
        first.protocol_version,
        first.generation
    );
    let health = service.health()?;
    println!(
        "health ready={} initialized={} sessions={} pid={}",
        health.ready, health.initialized, health.session_count, health.pid
    );

    let session_a = service.create_session()?;
    let session_b = service.create_session()?;
    println!(
        "two_sessions session_a={} session_b={} status={:?}",
        session_a,
        session_b,
        service.status()
    );
    service.close_session(&session_a)?;
    let crashed_pid = service
        .status()
        .process_id
        .ok_or("missing sidecar pid before crash gate")?;
    force_terminate(crashed_pid)?;
    for _ in 0..100 {
        if service.status().state != ComputerHostServiceState::Ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let crashed = service.status();
    println!("crash_detected status={crashed:?}");
    assert_eq!(crashed.state, ComputerHostServiceState::Crashed);
    assert!(service.close_session(&session_b).is_err());

    let restarted = service.restart_after_crash()?;
    println!("restarted status={restarted:?}");
    assert_eq!(restarted.state, ComputerHostServiceState::Ready);
    let new_session = service.create_session()?;
    service.close_session(&new_session)?;
    println!(
        "new_session_after_restart={} status={:?}",
        new_session,
        service.status()
    );

    service.shutdown()?;
    println!("shutdown status={:?}", service.status());
    service.shutdown()?;
    println!("repeated_shutdown status={:?}", service.status());
    assert_eq!(service.status().state, ComputerHostServiceState::Stopped);
    Ok(())
}
