use alice_computer_use_broker::{
    BrokerError, BrokerExecutionRequest, BrokerSessionRef, ComputerExecutionBroker,
    ComputerRequestOwner,
};
use alice_computer_use_core::{ComputerExecutionRequest, ElementId, SemanticAction};
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
    Err("R1 broker gate requires Windows".into())
}

fn path() -> PathBuf {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sidecar" {
            return PathBuf::from(args.next().expect("--sidecar requires a path"));
        }
    }
    PathBuf::from("target/release/alice-computer.exe")
}

fn owner(label: &str) -> ComputerRequestOwner {
    ComputerRequestOwner::new(
        format!("application-session-{label}"),
        format!("agent-thread-{label}"),
        format!("turn-{label}"),
        format!("tool-{label}"),
        None,
    )
}

fn same_session_different_owner(reference: &BrokerSessionRef) -> BrokerSessionRef {
    let mut forged = reference.clone();
    forged.owner = owner("forged");
    forged
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let host = ComputerHostService::new(ComputerHostServiceConfig::new(
        path(),
        Duration::from_secs(5),
    ));
    let broker = ComputerExecutionBroker::new(host);
    let first = broker.open_session(owner("one"))?;
    let second = broker.open_session(owner("two"))?;
    println!("two_sessions status={:?}", broker.status());

    let cross = broker.window_list(&same_session_different_owner(&first));
    assert!(matches!(cross, Err(BrokerError::CrossSessionReference)));
    println!("cross_session_reference=REJECTED");

    let lease = broker.acquire_desktop_lease(&first)?;
    assert!(matches!(
        broker.acquire_desktop_lease(&second),
        Err(BrokerError::DesktopBusy)
    ));
    println!("second_writer_lease=DESKTOP_BUSY");
    let request = BrokerExecutionRequest {
        request_id: "r1-exactly-once".into(),
        session: first.clone(),
        lease: lease.clone(),
        execution: ComputerExecutionRequest::semantic(SemanticAction::Focus {
            element_id: ElementId::new("r1-invalid-element"),
        }),
    };
    let first_result = broker.execute(request.clone());
    println!("first_dispatch_result={first_result:?}");
    let duplicate = broker.execute(request);
    assert!(matches!(duplicate, Err(BrokerError::DuplicateRequest(_))));
    println!("duplicate_dispatch=REJECTED");
    broker.release_desktop_lease(&lease).err();

    let pid = broker.status().process_id.ok_or("missing sidecar pid")?;
    force_terminate(pid)?;
    for _ in 0..100 {
        if broker.status().state != ComputerHostServiceState::Ready {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    println!("post_crash status={:?}", broker.status());
    assert!(matches!(
        broker.window_list(&first),
        Err(BrokerError::StaleReference { .. })
    ));
    println!("old_reference_after_restart=REJECTED");

    let restarted = broker.restart_after_crash()?;
    println!("restarted status={restarted:?}");
    let fresh = broker.open_session(owner("fresh"))?;
    assert!(matches!(
        broker.window_list(&first),
        Err(BrokerError::StaleReference { .. })
    ));
    broker.close_session(&second).err();
    broker.close_session(&fresh)?;
    broker.shutdown()?;
    println!("shutdown status={:?}", broker.status());
    Ok(())
}
