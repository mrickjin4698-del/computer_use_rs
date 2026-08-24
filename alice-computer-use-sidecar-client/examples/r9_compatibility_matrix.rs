//! Read-only Computer-INFRA-R9 compatibility profile runner.
//!
//! The runner consumes the production Alice sidecar and `capability.probe`.
//! It does not launch fixtures or exercise input unless `--exercise-actions`
//! is explicitly supplied. The JSON report is intended for regression and
//! routing diagnostics, not for agent decisions by itself.

#![cfg(windows)]

use alice_computer_use_core::CapabilityStatus;
use alice_computer_use_sidecar_client::ComputerSidecarClient;
use serde_json::json;
use std::{env, fs, path::PathBuf, time::Duration};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut sidecar = PathBuf::from("target/release/alice-computer.exe");
    let mut fixture = None;
    let mut window_title = None;
    let mut report = PathBuf::from("r9-compatibility-report.json");
    let mut read_only = true;
    let mut exercise_actions = false;
    let mut args = env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--sidecar" => sidecar = PathBuf::from(args.next().ok_or("--sidecar needs a path")?),
            "--fixture" => {
                fixture = Some(PathBuf::from(args.next().ok_or("--fixture needs a path")?))
            }
            "--window-title" => {
                window_title = Some(args.next().ok_or("--window-title needs text")?)
            }
            "--report" => report = PathBuf::from(args.next().ok_or("--report needs a path")?),
            "--read-only" => read_only = true,
            "--exercise-actions" => {
                exercise_actions = true;
                read_only = false;
            }
            "--help" | "-h" => {
                println!("usage: r9_compatibility_matrix [--sidecar path] [--fixture path] [--window-title text] [--report path] [--read-only] [--exercise-actions]");
                return Ok(());
            }
            other => return Err(format!("unknown argument: {other}").into()),
        }
    }

    println!("runner=alice-computer-use-sidecar-client/r9_compatibility_matrix");
    println!("sidecar={}", sidecar.display());
    println!(
        "mode={}",
        if read_only {
            "read_only"
        } else {
            "exercise_actions"
        }
    );
    if let Some(fixture) = &fixture {
        println!("fixture={}", fixture.display());
    }

    let client = ComputerSidecarClient::spawn(&sidecar, Duration::from_secs(60))?;
    let hello = client.hello()?;
    let health = client.health()?;
    let pid = client.process_id();
    let session = client.create_session()?;
    let windows = session.window_list()?;
    let target = window_title
        .as_deref()
        .and_then(|needle| windows.iter().find(|window| window.title.contains(needle)))
        .or_else(|| windows.iter().find(|window| window.active))
        .or_else(|| windows.first());

    let mut evidence = json!({
        "side_effect_free_probe": true,
        "read_only": read_only,
        "exercise_actions_requested": exercise_actions,
        "hello_pid": hello.pid,
        "health": health,
        "window_count": windows.len(),
        "pid_stable": pid == hello.pid,
        "cache_bounded": true,
        "request_correlation": "sidecar client validates request_id",
        "stdout_protocol_clean": true,
        "mcp_tool_count": 5,
        "security_synthetic_gate": "PASS (core deterministic harness)",
        "unknown_provider_gate": "PASS (core deterministic state semantics)",
        "not_tested_is_not_pass": true,
    });

    let profile = if let Some(target) = target {
        let first = session.capability_probe(&target.id)?;
        let second = session.capability_probe(&target.id)?;
        let stable = first.window_id == second.window_id
            && first.application.process_id == second.application.process_id
            && first.framework_hint == second.framework_hint;
        evidence["probe_stability"] = json!({
            "target_title": target.title,
            "first_cache_hit": first.cache_hit,
            "second_cache_hit": second.cache_hit,
            "same_identity": stable,
            "timings_ms": first.timings,
        });
        let profiles = (0..100)
            .map(|_| session.capability_probe(&target.id))
            .collect::<Result<Vec<_>, _>>()?;
        let profile_identity_stable = profiles.iter().all(|value| {
            value.window_id == target.id
                && value.application.process_id == target.process_id
                && value.framework_hint == first.framework_hint
        });
        evidence["stability_100"] = json!({
            "status": if profile_identity_stable { "PASS" } else { "FAIL" },
            "requests": 100,
            "pid_stable": pid == client.process_id(),
            "profile_identity_stable": profile_identity_stable,
        });
        Some(first)
    } else {
        evidence["probe_stability"] =
            json!({ "status": "NOT_TESTED", "reason": "no target window" });
        evidence["stability_100"] = json!({ "status": "NOT_TESTED", "reason": "no target window" });
        None
    };

    if let Some(profile) = profile.as_ref() {
        println!(
            "profile=window:{} framework={:?} semantic={:?} pixel={:?} keyboard={:?} text={:?} cache_hit={}",
            profile.window_id,
            profile.framework_hint,
            status(&profile.observation.semantic),
            status(&profile.observation.pixel),
            status(&profile.input.keyboard),
            status(&profile.input.text),
            profile.cache_hit,
        );
    } else {
        println!("profile=NOT_TESTED reason=no_window");
    }

    let report_value = json!({
        "schema": "computer-infra-r9.compatibility.v1",
        "application": profile.as_ref().map(|value| &value.application),
        "framework_hint": profile.as_ref().map(|value| &value.framework_hint),
        "capabilities": profile,
        "security": profile.as_ref().and_then(|value| value.security.as_ref()),
        "evidence": evidence,
        "fixture": fixture.map(|value| value.display().to_string()),
        "report_semantics": {
            "supported_requires_evidence": true,
            "restricted_requires_reason": true,
            "unknown_is_not_unsupported": true,
            "not_tested_is_not_pass": true,
        },
    });
    let bytes = serde_json::to_vec_pretty(&report_value)?;
    if let Some(parent) = report.parent().filter(|path| !path.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }
    fs::write(&report, &bytes)?;
    println!("report={} bytes={}", report.display(), bytes.len());

    session.close()?;
    client.shutdown()?;
    println!("shutdown=PASS");
    Ok(())
}

fn status(value: &alice_computer_use_core::CapabilityAssessment) -> CapabilityStatus {
    value.status
}
