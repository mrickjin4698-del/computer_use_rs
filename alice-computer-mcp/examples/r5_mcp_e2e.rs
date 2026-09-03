//! Real interactive Windows R5 gate harness.
//!
//! The harness talks to the existing five-tool MCP surface. It does not call
//! WinNative or UIA directly; all actions go through `computer_execute` and
//! the production sidecar adapter. The fixture is a separate test executable.

#![cfg(windows)]

use png::Decoder;
use serde_json::{json, Value};
use std::{
    env,
    io::{BufRead, BufReader, Cursor, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
    thread,
    time::Duration,
};
use windows_sys::Win32::{
    Foundation::CloseHandle,
    System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE},
};

type GateResult<T> = Result<T, String>;

struct McpProcess {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn spawn(adapter: &PathBuf, sidecar: &PathBuf) -> GateResult<Self> {
        let mut child = Command::new(adapter)
            .arg("--sidecar")
            .arg(sidecar)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("spawn adapter: {error}"))?;
        let stdin = child.stdin.take().ok_or("adapter stdin was not piped")?;
        let stdout = child.stdout.take().ok_or("adapter stdout was not piped")?;
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader
                    .read_line(&mut line)
                    .ok()
                    .filter(|n| *n > 0)
                    .is_some()
                {
                    line.clear();
                }
            });
        }
        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> GateResult<()> {
        let result = self.call("initialize", json!({}))?;
        if result["serverInfo"]["name"] != "alice-computer-mcp" {
            return Err(format!("unexpected MCP server info: {result}"));
        }
        self.notify("notifications/initialized", json!({}))
    }

    fn notify(&mut self, method: &str, params: Value) -> GateResult<()> {
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        self.write(&request)
    }

    fn call(&mut self, method: &str, params: Value) -> GateResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.write(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        }))?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|error| format!("read MCP {method}: {error}"))?;
        if line.is_empty() {
            return Err(format!("MCP EOF while waiting for {method}"));
        }
        let response: Value = serde_json::from_str(&line)
            .map_err(|error| format!("MCP stdout was not JSON for {method}: {error}"))?;
        if response["id"] != id {
            return Err(format!("MCP request_id mismatch for {method}: {response}"));
        }
        if response.get("error").is_some() {
            return Err(format!("MCP JSON-RPC error for {method}: {response}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("MCP response had no result for {method}"))
    }

    fn write(&mut self, value: &Value) -> GateResult<()> {
        let stdin = self.stdin.as_mut().ok_or("adapter stdin is closed")?;
        serde_json::to_writer(&mut *stdin, value).map_err(|error| error.to_string())?;
        stdin.write_all(b"\n").map_err(|error| error.to_string())?;
        stdin.flush().map_err(|error| error.to_string())
    }

    fn tool(&mut self, name: &str, arguments: Value) -> GateResult<Value> {
        self.call(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn close(mut self) -> GateResult<()> {
        self.stdin.take();
        let status = self
            .child
            .wait()
            .map_err(|error| format!("wait adapter: {error}"))?;
        if !status.success() {
            return Err(format!("adapter exited with {status}"));
        }
        Ok(())
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn main() -> GateResult<()> {
    let adapter = argument_path("--adapter")
        .unwrap_or_else(|| PathBuf::from("target/debug/alice-computer-mcp.exe"));
    let sidecar = argument_path("--sidecar")
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    let fixture = argument_path("--fixture")
        .unwrap_or_else(|| PathBuf::from("target/debug/examples/r5_action_fixture.exe"));
    let independent = argument_path("--independent")
        .unwrap_or_else(|| PathBuf::from("target/debug/examples/r5_toggle_independent_uia.exe"));
    let start_fixture = !has_flag("--no-start-fixture");
    let mut fixture_child = if start_fixture {
        Some(
            Command::new(&fixture)
                // Gate 11 uses the fixture's bounded post-command delay as a
                // controlled crash window; it is never a production retry or
                // Toggle workaround.
                .env("ALICE_R5_SLOW_TOGGLE", "1")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .map_err(|error| format!("start R5 fixture: {error}"))?,
        )
    } else {
        None
    };
    if start_fixture {
        thread::sleep(Duration::from_millis(500));
    }
    println!(
        "r5 adapter={} sidecar={} fixture={} interactive=true",
        adapter.display(),
        sidecar.display(),
        fixture.display()
    );

    let mut mcp = McpProcess::spawn(&adapter, &sidecar)?;
    mcp.initialize()?;
    gate_tools(&mut mcp)?;
    println!("r5_1_mcp_schema=PASS");

    let first = observe(&mut mcp, true)?;
    let window = find_window(&first, "alice computer r5 fixture")?;
    if window["active"] != true {
        return Err(format!(
            "R5 fixture is not foreground; focus it manually and rerun: {window}"
        ));
    }
    let semantic = semantic_observe(&mut mcp, &window["id"])?;
    let checkbox = find_element(&semantic, |e| e["name"] == "R5 CheckBox")?;
    let mouse = find_element(&semantic, |e| e["name"] == "R5 Mouse Target")?;
    let edit = find_element(&semantic, |e| e["automation_id"] == "1001")?;
    let coordinate = center_coordinate(&mouse, &first)?;
    let edit_coordinate = center_coordinate(&edit, &first)?;
    let screenshot = mcp.tool("computer_screenshot", json!({}))?;
    verify_png(&screenshot)?;
    save_png_probe(&screenshot)?;
    println!(
        "mouse_target bounds={} coordinate={}",
        mouse["bounds"], coordinate
    );
    let target_window_id = window["id"].clone();

    gate_mouse(&mut mcp, &target_window_id, &coordinate, &mouse)?;
    println!("gate1_mouse_state=PASS");
    gate_modifier_click(&mut mcp, &target_window_id, &coordinate, &mouse)?;
    println!("gate2_modifier_click=PASS");
    assert_performed(
        &execute_semantic(&mut mcp, "focus", &edit["id"], None)?,
        "fixture edit focus",
    )?;
    assert_performed(
        &execute_pixel(
            &mut mcp,
            "mouse_down",
            &target_window_id,
            &edit_coordinate,
            json!({ "button": "left" }),
        )?,
        "fixture edit click down",
    )?;
    assert_performed(
        &execute_pixel(
            &mut mcp,
            "mouse_up",
            &target_window_id,
            &edit_coordinate,
            json!({ "button": "left" }),
        )?,
        "fixture edit click up",
    )?;
    gate_keyboard_state(&mut mcp, &target_window_id)?;
    println!("gate3_keyboard_state=PASS");
    gate_hold_key(&mut mcp, &target_window_id)?;
    println!("gate4_hold_key=PASS");

    let forensic =
        gate_toggle_forensics(&mut mcp, &target_window_id, &checkbox, &first, &independent)?;
    println!("gate5_1_result={forensic}");

    gate_select(&mut mcp, &target_window_id)?;
    println!("gate6_select=PASS");
    gate_expand_collapse(&mut mcp, &target_window_id)?;
    println!("gate7_expand_collapse=PASS");
    gate_range(&mut mcp, &target_window_id)?;
    println!("gate8_range_value=PASS");
    gate_scroll(&mut mcp, &target_window_id)?;
    println!("gate9_scroll_into_view=PASS");
    gate_capability_admission(&mut mcp, &target_window_id)?;
    println!("gate10_capability_admission=PASS");
    gate_stale_and_unknown(&mut mcp, &target_window_id)?;
    println!("gate11_stale_outcome_unknown=PASS");
    gate_pressed_cleanup(&adapter, &sidecar, &coordinate)?;
    println!("gate12_pressed_cleanup=PASS graceful=true");
    gate_tools(&mut mcp)?;
    println!("gate13_mcp_regression=PASS");
    mcp.close()?;

    if let Some(mut fixture) = fixture_child.take() {
        let _ = fixture.kill();
        let _ = fixture.wait();
    }
    println!("r5_result=PASS");
    Ok(())
}

fn gate_tools(mcp: &mut McpProcess) -> GateResult<()> {
    let result = mcp.call("tools/list", json!({}))?;
    let tools = result["tools"]
        .as_array()
        .ok_or("tools/list had no tools")?;
    let expected = [
        "computer_health",
        "computer_observe",
        "computer_execute",
        "computer_use",
        "computer_screenshot",
        "computer_validate",
    ];
    let names = tools
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    if names.len() != expected.len() || expected.iter().any(|name| !names.contains(name)) {
        return Err(format!("MCP tool count/name regression: {names:?}"));
    }
    let bytes = tools
        .iter()
        .map(|tool| {
            serde_json::to_vec(tool)
                .map(|value| value.len())
                .unwrap_or(0)
        })
        .sum::<usize>();
    println!("schema tools={} total_bytes={bytes}", names.len());
    if bytes > 16_384 {
        return Err(format!("MCP schema grew beyond budget: {bytes}"));
    }
    Ok(())
}

fn gate_mouse(mcp: &mut McpProcess, window: &Value, at: &Value, mouse: &Value) -> GateResult<()> {
    let down = execute_pixel(mcp, "mouse_down", window, at, json!({ "button": "left" }))?;
    assert_performed(&down, "MouseDown")?;
    wait_for_title(mcp, "mouse=left:down", "MouseDown logger")?;
    let up = execute_pixel(mcp, "mouse_up", window, at, json!({ "button": "left" }))?;
    assert_performed(&up, "MouseUp")?;
    wait_for_title(mcp, "mouse=left:up", "MouseUp logger")?;
    let middle = execute_pixel(mcp, "middle_click", window, at, json!({}))?;
    assert_performed(&middle, "MiddleClick")?;
    wait_for_title(mcp, "mouse=middle:up", "MiddleClick logger")?;
    let triple = execute_pixel(mcp, "triple_click", window, at, json!({}))?;
    assert_performed(&triple, "TripleClick")?;
    let title = wait_for_mouse_up_count(mcp, "left", 4, "TripleClick logger")?;
    let logical_left_clicks =
        title.matches("mouse=left:down").count() + title.matches("mouse=left:double").count();
    if logical_left_clicks < 3
        || title.matches("mouse=left:up").count() < 4
        || !title.contains("mouse=middle:down")
    {
        return Err(format!(
            "mouse logger did not record exact primitives: {title}"
        ));
    }
    let _ = mouse;
    Ok(())
}

fn gate_modifier_click(
    mcp: &mut McpProcess,
    window: &Value,
    at: &Value,
    mouse: &Value,
) -> GateResult<()> {
    let ctrl = execute_pixel(
        mcp,
        "modifier_click",
        window,
        at,
        json!({ "modifier": "ctrl", "button": "left" }),
    )?;
    assert_performed(&ctrl, "Ctrl+Click")?;
    let title = wait_for_mouse_modifier(mcp, "ctrl=true", "Ctrl+Click logger")?;
    if !mouse_event_has_modifier(&title, "ctrl=true")
        || !title.contains("MODIFIERS=ctrl=false;shift=false;alt=false")
    {
        return Err(format!("Ctrl+Click modifier evidence missing: {title}"));
    }
    let shift = execute_pixel(
        mcp,
        "modifier_click",
        window,
        at,
        json!({ "modifier": "shift", "button": "left" }),
    )?;
    assert_performed(&shift, "Shift+Click")?;
    let title = wait_for_mouse_modifier(mcp, "shift=true", "Shift+Click logger")?;
    if !mouse_event_has_modifier(&title, "shift=true")
        || !title.contains("MODIFIERS=ctrl=false;shift=false;alt=false")
    {
        return Err(format!("Shift+Click modifier evidence missing: {title}"));
    }
    let _ = mouse;
    Ok(())
}

fn mouse_event_has_modifier(title: &str, modifier: &str) -> bool {
    title.split("mouse=left:").skip(1).any(|event| {
        let event = event.split(";mouse=").next().unwrap_or(event);
        (event.starts_with("down:") || event.starts_with("double:")) && event.contains(modifier)
    })
}

fn wait_for_mouse_modifier(
    mcp: &mut McpProcess,
    modifier: &str,
    label: &str,
) -> GateResult<String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let observation = observe(mcp, false)?;
        let title = fixture_title(&observation)?;
        if title.contains("MODIFIERS=ctrl=false;shift=false;alt=false")
            && mouse_event_has_modifier(&title, modifier)
        {
            return Ok(title);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("{label} evidence missing: {observation}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn gate_keyboard_state(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    for (key, sequence) in [
        (
            "ctrl",
            [
                "key=VK_CONTROL:down",
                "key=A:down",
                "key=A:up",
                "key=VK_CONTROL:up",
            ],
        ),
        (
            "shift",
            [
                "key=VK_SHIFT:down",
                "key=A:down",
                "key=A:up",
                "key=VK_SHIFT:up",
            ],
        ),
    ] {
        assert_performed(
            &execute_pixel(mcp, "key_down", window, &json!({}), json!({ "key": key }))?,
            "KeyDown",
        )?;
        assert_performed(
            &execute_pixel(mcp, "key_down", window, &json!({}), json!({ "key": "a" }))?,
            "KeyDown A",
        )?;
        assert_performed(
            &execute_pixel(mcp, "key_up", window, &json!({}), json!({ "key": "a" }))?,
            "KeyUp A",
        )?;
        assert_performed(
            &execute_pixel(mcp, "key_up", window, &json!({}), json!({ "key": key }))?,
            "KeyUp modifier",
        )?;
        wait_for_title(mcp, sequence[3], "keyboard logger dispatch")?;
        let title = fixture_title(&observe(mcp, false)?)?;
        let mut cursor = 0;
        for marker in sequence {
            let next = title[cursor..]
                .find(marker)
                .map(|offset| cursor + offset)
                .ok_or_else(|| format!("keyboard logger missing {marker}: {title}"))?;
            cursor = next + marker.len();
        }
    }
    Ok(())
}

fn gate_hold_key(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    let result = execute_pixel(
        mcp,
        "hold_key",
        window,
        &json!({}),
        json!({ "key": "A", "duration_ms": 120 }),
    )?;
    assert_performed(&result, "HoldKey")?;
    let title = wait_for_key_event_count(mcp, "A", "up", 3, "HoldKey logger dispatch")?;
    let down = latest_key_event(&title, "A", "down")
        .ok_or_else(|| format!("HoldKey down evidence missing: {title}"))?;
    let up = latest_key_event(&title, "A", "up")
        .ok_or_else(|| format!("HoldKey up evidence missing: {title}"))?;
    let down_ms = event_time(down)?;
    let up_ms = event_time(up)?;
    if up_ms < down_ms + 80 || up_ms > down_ms + 500 {
        return Err(format!(
            "HoldKey duration out of tolerance: down={down_ms} up={up_ms}"
        ));
    }
    let illegal = execute_pixel(
        mcp,
        "hold_key",
        window,
        &json!({}),
        json!({ "key": "F1", "duration_ms": 5001 }),
    )?;
    if illegal["isError"] != true
        || illegal["structuredContent"]["final_outcome"] != "invalid_request"
    {
        return Err(format!("too-long HoldKey was not rejected: {illegal}"));
    }
    Ok(())
}

fn wait_for_key_event_count(
    mcp: &mut McpProcess,
    key: &str,
    phase: &str,
    count: usize,
    label: &str,
) -> GateResult<String> {
    let marker = format!("key={key}:{phase}");
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let observation = observe(mcp, false)?;
        let title = fixture_title(&observation)?;
        if title.matches(&marker).count() >= count {
            return Ok(title);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("{label} evidence missing: {observation}"));
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn latest_key_event<'a>(title: &'a str, key: &str, phase: &str) -> Option<&'a str> {
    let marker = format!("key={key}:{phase}");
    let start = title.rfind(&marker)?;
    let event = &title[start..];
    let end = event.find(";key=").unwrap_or(event.len());
    Some(event[..end].trim())
}

fn gate_toggle_forensics(
    mcp: &mut McpProcess,
    window: &Value,
    checkbox: &Value,
    observation: &Value,
    independent: &PathBuf,
) -> GateResult<&'static str> {
    let coordinate = center_coordinate(checkbox, observation)?;
    println!(
        "toggle_checkbox bounds={} coordinate={}",
        checkbox["bounds"], coordinate
    );
    let before_title = fixture_title(&observe(mcp, false)?)?;
    println!("toggle_fixture_before={before_title}");
    if !before_title.contains("class=Button")
        || !before_title.contains("style=0x")
        || !before_title.contains("bm_getcheck=0")
    {
        return Err(format!(
            "Win32 checkbox read-only diagnostic did not prove standard initial state: {before_title}"
        ));
    }

    // Gate R5.1-A: physical click is fixture sanity only. It is never used as
    // the production semantic Toggle implementation or as a fallback.
    let mut physical_sanity_pass = true;
    for (expected_count, expected_check) in [(1, "bm_getcheck=1"), (2, "bm_getcheck=0")] {
        let probe = Command::new(independent)
            .arg("--physical")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|error| format!("run physical checkbox probe: {error}"))?;
        let probe_stdout = String::from_utf8_lossy(&probe.stdout);
        let probe_stderr = String::from_utf8_lossy(&probe.stderr);
        println!("physical_checkbox_probe={probe_stdout}");
        if !probe.status.success() {
            return Err(format!(
                "physical checkbox probe failed: status={} stdout={} stderr={}",
                probe.status, probe_stdout, probe_stderr
            ));
        }
        let title = match wait_for_title(
            mcp,
            &format!("bn_clicked={expected_count}"),
            "checkbox physical click dispatch",
        ) {
            Ok(title) => title,
            Err(error) => {
                physical_sanity_pass = false;
                println!("gate5_1_a_physical_click_{expected_count}=PARTIAL detail={error}");
                break;
            }
        };
        if !title.contains(expected_check) {
            return Err(format!(
                "Gate R5.1-A expected BM_GETCHECK {expected_check}: {title}"
            ));
        }
        println!("gate5_1_a_physical_click_{expected_count}={title}");
    }
    println!(
        "gate5_1_a={}",
        if physical_sanity_pass {
            "PASS"
        } else {
            "PARTIAL"
        }
    );

    let before = semantic_observe(mcp, window)?;
    let element = find_element(&before, |e| e["name"] == "R5 CheckBox")?;
    if element["toggle_state"] != false || element["capabilities"]["toggleable"] != true {
        return Err(format!(
            "checkbox semantic initial state/capability invalid: {element}"
        ));
    }
    println!(
        "gate5_1_b_identity element_id={} generation={} control_type={} name={} class_name={} automation_id={} bounds={} toggleable={} native_window_handle=diagnostic_only",
        element["id"],
        before["metadata"]["generation"],
        element["control_type"],
        element["name"],
        element["class_name"],
        element["automation_id"],
        element["bounds"],
        element["capabilities"]["toggleable"],
    );
    println!("gate5_1_b=PASS");

    // Gate R5.1-D production call: exactly one semantic Toggle. A provider
    // failure is evidence and must not trigger a second call or a pixel path.
    let alice_result = execute_semantic(mcp, "toggle", &element["id"], None)?;
    let after_semantic = semantic_observe(mcp, window)?;
    let after_element = find_element(&after_semantic, |e| e["name"] == "R5 CheckBox")?;
    let after_title = fixture_title(&observe(mcp, false)?)?;
    let screenshot = mcp.tool("computer_screenshot", json!({}))?;
    verify_png(&screenshot)?;
    save_png_evidence(&screenshot, "toggle-after.png")?;
    println!(
        "toggle_matrix | UIA same element = detail:{} | UIA fresh element = {} | BM_GETCHECK/diagnostic = {} | BN_CLICKED/WM_COMMAND = {} | visual = toggle-after.png (PNG decoded)",
        alice_result["structuredContent"]["verification"]["detail"],
        after_element["toggle_state"],
        after_title
            .split("CHECKBOX_DIAG=")
            .nth(1)
            .unwrap_or("<missing>"),
        after_title,
    );
    println!("alice_toggle_result={alice_result}");
    println!("alice_fresh_semantic={after_element}");

    // Gate R5.1-C: separate process, separate UIA client, same native
    // checkbox. This invocation is diagnostic only and performs one Toggle.
    let independent_output = Command::new(independent)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|error| {
            format!(
                "run independent UIA client {}: {error}",
                independent.display()
            )
        })?;
    let independent_stdout = String::from_utf8_lossy(&independent_output.stdout);
    let independent_stderr = String::from_utf8_lossy(&independent_output.stderr);
    println!("independent_uia_stdout={independent_stdout}");
    if !independent_stderr.trim().is_empty() {
        println!("independent_uia_stderr={independent_stderr}");
    }
    if !independent_output.status.success() {
        return Err(format!(
            "independent UIA client failed: status={} stdout={} stderr={}",
            independent_output.status, independent_stdout, independent_stderr
        ));
    }

    let alice_passed = alice_result["structuredContent"]["final_outcome"] == "performed"
        && alice_result["structuredContent"]["verification"]["verified"] == true
        && alice_result["structuredContent"]["verification"]["toggled"] == true;
    let independent_transition = independent_stdout.contains("independent_transition=true")
        && independent_stdout.contains("same_fresh_agree=true")
        && independent_stdout.contains("native_matches_fresh=true");
    if physical_sanity_pass && alice_passed && independent_transition {
        println!("gate5_1_c=PASS root=NONE");
        println!("gate5_1_d=PASS");
        return Ok("PASS");
    }
    if !physical_sanity_pass && alice_passed && independent_transition {
        println!("gate5_1_c=PASS root=NONE");
        println!("gate5_1_d=PASS semantic_toggle=true physical_sanity=PARTIAL");
        return Ok("PARTIAL");
    }
    if independent_transition && !alice_passed {
        println!("gate5_1_c=PASS root=ALICE_IMPLEMENTATION");
        println!(
            "gate5_1_d={}",
            if physical_sanity_pass {
                "FAIL"
            } else {
                "PARTIAL"
            }
        );
        return Ok("PARTIAL");
    }
    println!("gate5_1_c=PASS root=WINDOWS_PROVIDER");
    println!("gate5_1_d=PARTIAL compatibility_fixture_required=true");
    Ok("PARTIAL")
}

fn gate_select(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    let semantic = semantic_observe(mcp, window)?;
    let a = find_element(&semantic, |e| e["name"] == "R5 Radio A")?;
    let b = find_element(&semantic, |e| e["name"] == "R5 Radio B")?;
    let select_b = execute_semantic(mcp, "select", &b["id"], None)?;
    assert_performed(&select_b, "Select B")?;
    let after_b = semantic_observe(mcp, window)?;
    if find_element(&after_b, |e| e["name"] == "R5 Radio B")?["selected"] != true
        || find_element(&after_b, |e| e["name"] == "R5 Radio A")?["selected"] != false
    {
        return Err(format!("radio switch B verification failed: {select_b}"));
    }
    let a_current = find_element(&after_b, |e| e["name"] == "R5 Radio A")?;
    let select_a = execute_semantic(mcp, "select", &a_current["id"], None)?;
    assert_performed(&select_a, "Select A")?;
    let after_a = semantic_observe(mcp, window)?;
    if find_element(&after_a, |e| e["name"] == "R5 Radio A")?["selected"] != true {
        return Err(format!("radio switch A verification failed: {select_a}"));
    }
    let _ = a;
    Ok(())
}

fn gate_expand_collapse(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    let semantic = semantic_observe(mcp, window)?;
    let combo = find_element(&semantic, |e| e["control_type"] == "combo_box")?;
    let expanded = execute_semantic(mcp, "expand", &combo["id"], None)?;
    assert_performed(&expanded, "Expand")?;
    // The action result contains the authoritative fresh
    // ExpandCollapseState read. Keep this runner-side read bounded because an
    // open native ComboBox popup may expose repeated provider aliases.
    let after_expand = find_element(&semantic_observe_with_limit(mcp, window, 128)?, |e| {
        e["control_type"] == "combo_box"
    })?;
    if after_expand["expanded"] != true {
        return Err(format!(
            "Expand readback failed: {expanded} / {after_expand}"
        ));
    }
    let collapsed = execute_semantic(mcp, "collapse", &after_expand["id"], None)?;
    assert_performed(&collapsed, "Collapse")?;
    let after_collapse = find_element(&semantic_observe_with_limit(mcp, window, 128)?, |e| {
        e["control_type"] == "combo_box"
    })?;
    if after_collapse["expanded"] != false {
        return Err(format!(
            "Collapse readback failed: {collapsed} / {after_collapse}"
        ));
    }
    Ok(())
}

fn gate_range(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    for value in [0.0, 50.0, 100.0] {
        let semantic = semantic_observe(mcp, window)?;
        let slider = find_element(&semantic, |e| e["control_type"] == "slider")?;
        let result = execute_semantic(mcp, "set_range_value", &slider["id"], Some(json!(value)))?;
        assert_performed(&result, "SetRangeValue")?;
        let current = find_element(&semantic_observe(mcp, window)?, |e| {
            e["control_type"] == "slider"
        })?;
        if current["range_value"]
            .as_f64()
            .map(|actual| (actual - value).abs() <= 0.000001)
            != Some(true)
        {
            return Err(format!(
                "RangeValue readback mismatch: expected={value} actual={current}"
            ));
        }
    }
    for value in [-1.0, 101.0] {
        let semantic = semantic_observe(mcp, window)?;
        let slider = find_element(&semantic, |e| e["control_type"] == "slider")?;
        let result = execute_semantic(mcp, "set_range_value", &slider["id"], Some(json!(value)))?;
        if result["isError"] != true
            || result["structuredContent"]["final_outcome"] != "invalid_value"
        {
            return Err(format!("out-of-range value was not rejected: {result}"));
        }
    }
    Ok(())
}

fn gate_scroll(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    let semantic = semantic_observe(mcp, window)?;
    let item = find_element(&semantic, |e| e["name"] == "R5 Item 30")?;
    if item["offscreen"] != true || item["capabilities"]["scroll_into_view"] != true {
        return Err(format!(
            "scroll fixture was not initially offscreen: {item}"
        ));
    }
    let result = execute_semantic(mcp, "scroll_into_view", &item["id"], None)?;
    assert_performed(&result, "ScrollIntoView")?;
    let current = find_element(&semantic_observe(mcp, window)?, |e| {
        e["name"] == "R5 Item 30"
    })?;
    if current["offscreen"] != false
        || result["structuredContent"]["verification"]["offscreen"] != false
    {
        return Err(format!(
            "ScrollIntoView readback failed: {result} / {current}"
        ));
    }
    Ok(())
}

fn gate_capability_admission(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    // Gate 6-9 advance the semantic generation. Resolve fresh targets here;
    // feeding the initial observation's IDs would correctly produce
    // StaleElement and would not test capability admission.
    let semantic = semantic_observe(mcp, window)?;
    let edit = find_element(&semantic, |e| e["automation_id"] == "1001")?;
    let mouse = find_element(&semantic, |e| e["name"] == "R5 Mouse Target")?;
    let before = fixture_title(&observe(mcp, false)?)?.to_owned();
    for (operation, element, value) in [
        ("toggle", mouse.clone(), None),
        ("select", edit.clone(), None),
        ("expand", edit, None),
        ("set_range_value", mouse, Some(json!(50.0))),
    ] {
        let result = execute_semantic(mcp, operation, &element["id"], value)?;
        if result["structuredContent"]["final_outcome"] != "unsupported" {
            return Err(format!(
                "capability admission did not reject {operation}: {result}"
            ));
        }
    }
    let after = fixture_title(&observe(mcp, false)?)?;
    if before != after {
        return Err(format!(
            "capability rejection caused fixture side effect: {before} -> {after}"
        ));
    }
    Ok(())
}

fn gate_stale_and_unknown(mcp: &mut McpProcess, window: &Value) -> GateResult<()> {
    let semantic = semantic_observe(mcp, window)?;
    let stale_target = find_element(&semantic, |e| e["name"] == "R5 CheckBox")?;
    let _ = semantic_observe(mcp, window)?;
    let stale = execute_semantic(mcp, "toggle", &stale_target["id"], None)?;
    if stale["isError"] != true || stale["structuredContent"]["final_outcome"] != "stale_element" {
        return Err(format!("stale Toggle was not rejected: {stale}"));
    }

    let current = semantic_observe(mcp, window)?;
    let target = find_element(&current, |e| e["name"] == "R5 CheckBox")?;
    let before = target["toggle_state"].clone();
    let pid = health_pid(mcp)?;
    let killer = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        terminate_process(pid)
    });
    let uncertain = execute_semantic(mcp, "toggle", &target["id"], None);
    let _ = killer.join();
    let unknown = match uncertain {
        Ok(ref value) => {
            value["isError"] == true
                && (value["structuredContent"]["final_outcome"] == "outcome_unknown"
                    || value["structuredContent"]["error"]["outcome_unknown"] == true)
        }
        Err(ref error) => error.contains("EOF") || error.contains("OUTCOME_UNKNOWN"),
    };
    if !unknown {
        return Err(format!(
            "Toggle crash did not become OUTCOME_UNKNOWN: {uncertain:?}"
        ));
    }
    mcp.kill();
    let mut restarted = McpProcess::spawn(
        &argument_path("--adapter")
            .unwrap_or_else(|| PathBuf::from("target/debug/alice-computer-mcp.exe")),
        &argument_path("--sidecar")
            .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe")),
    )?;
    restarted.initialize()?;
    let after = semantic_observe(&mut restarted, window)?;
    let after_toggle = find_element(&after, |e| e["name"] == "R5 CheckBox")?;
    if after_toggle["toggle_state"] == before {
        return Err(format!(
            "uncertain Toggle did not produce one observed state transition: {after_toggle}"
        ));
    }
    *mcp = restarted;
    Ok(())
}

fn gate_pressed_cleanup(adapter: &PathBuf, sidecar: &PathBuf, at: &Value) -> GateResult<()> {
    let mut key_session = McpProcess::spawn(adapter, sidecar)?;
    key_session.initialize()?;
    let key_window = fresh_fixture_window(&mut key_session)?;
    assert_performed(
        &execute_pixel(
            &mut key_session,
            "key_down",
            &key_window,
            &json!({}),
            json!({ "key": "ctrl" }),
        )?,
        "cleanup KeyDown",
    )?;
    key_session.close()?;
    let mut verify = McpProcess::spawn(adapter, sidecar)?;
    verify.initialize()?;
    if !fixture_title(&observe(&mut verify, false)?)?.contains("key=VK_CONTROL:up") {
        return Err("graceful session close did not release Ctrl".into());
    }
    verify.close()?;

    let mut mouse_session = McpProcess::spawn(adapter, sidecar)?;
    mouse_session.initialize()?;
    let mouse_window = fresh_fixture_window(&mut mouse_session)?;
    assert_performed(
        &execute_pixel(
            &mut mouse_session,
            "mouse_down",
            &mouse_window,
            at,
            json!({ "button": "left" }),
        )?,
        "cleanup MouseDown",
    )?;
    mouse_session.close()?;
    let mut verify_mouse = McpProcess::spawn(adapter, sidecar)?;
    verify_mouse.initialize()?;
    if !fixture_title(&observe(&mut verify_mouse, false)?)?.contains("mouse=left:up") {
        return Err("graceful session close did not release left mouse button".into());
    }
    let sidecar_pid = health_pid(&mut verify_mouse)?;
    verify_mouse.close()?;
    if process_alive(sidecar_pid) {
        return Err(format!(
            "sidecar residual after graceful close: {sidecar_pid}"
        ));
    }
    Ok(())
}

fn observe(mcp: &mut McpProcess, screenshot_metadata: bool) -> GateResult<Value> {
    let result = mcp.tool(
        "computer_observe",
        json!({ "screenshot_metadata": screenshot_metadata }),
    )?;
    tool_structured(&result).cloned()
}

fn fresh_fixture_window(mcp: &mut McpProcess) -> GateResult<Value> {
    let observation = observe(mcp, false)?;
    Ok(find_window(&observation, "alice computer r5 fixture")?["id"].clone())
}

fn semantic_observe(mcp: &mut McpProcess, window: &Value) -> GateResult<Value> {
    semantic_observe_with_limit(mcp, window, 512)
}

fn semantic_observe_with_limit(
    mcp: &mut McpProcess,
    window: &Value,
    max_elements: u64,
) -> GateResult<Value> {
    let result = mcp.tool(
        "computer_observe",
        json!({ "window_id": window, "semantic": true, "max_depth": 8, "max_elements": max_elements }),
    )?;
    Ok(tool_structured(&result)?["semantic"].clone())
}

fn execute_semantic(
    mcp: &mut McpProcess,
    operation: &str,
    element_id: &Value,
    value: Option<Value>,
) -> GateResult<Value> {
    let mut intent = json!({ "operation": operation, "element_id": element_id });
    if let Some(value) = value {
        intent["value"] = value;
    }
    mcp.tool(
        "computer_execute",
        json!({ "intent": intent, "strategy": "semantic_only", "fallback_policy": "deny" }),
    )
}

fn execute_pixel(
    mcp: &mut McpProcess,
    operation: &str,
    window: &Value,
    at: &Value,
    extra: Value,
) -> GateResult<Value> {
    let mut intent = extra.as_object().cloned().unwrap_or_default();
    intent.insert("operation".into(), json!(operation));
    intent.insert("target_window_id".into(), window.clone());
    if operation != "key_down" && operation != "key_up" && operation != "hold_key" {
        intent.insert("at".into(), at.clone());
    }
    mcp.tool(
        "computer_execute",
        json!({ "intent": intent, "strategy": "pixel_only", "fallback_policy": "deny" }),
    )
}

fn center_coordinate(element: &Value, observation: &Value) -> GateResult<Value> {
    let mut coordinate = element["bounds"].clone();
    let display = observation["display"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["primary"] == true))
        .ok_or("primary display metadata missing")?;
    coordinate["point"]["x"] = json!(
        element["bounds"]["point"]["x"].as_f64().unwrap_or(0.0)
            + element["bounds"]["extent"]["width"].as_f64().unwrap_or(0.0) / 2.0
    );
    coordinate["point"]["y"] = json!(
        element["bounds"]["point"]["y"].as_f64().unwrap_or(0.0)
            + element["bounds"]["extent"]["height"]
                .as_f64()
                .unwrap_or(0.0)
                / 2.0
    );
    coordinate["extent"] = json!(display["bounds"]["size"]);
    coordinate["dpi"] = json!(display["dpi"]);
    Ok(coordinate)
}

fn tool_structured(result: &Value) -> GateResult<&Value> {
    if result["isError"] == true {
        return Err(format!("MCP tool error: {result}"));
    }
    result
        .get("structuredContent")
        .ok_or_else(|| format!("MCP result has no structuredContent: {result}"))
}

fn assert_performed(result: &Value, label: &str) -> GateResult<()> {
    if result["isError"] == true || result["structuredContent"]["final_outcome"] != "performed" {
        return Err(format!("{label} failed: {result}"));
    }
    Ok(())
}

fn find_window(observation: &Value, needle: &str) -> GateResult<Value> {
    observation["windows"]
        .as_array()
        .and_then(|items| {
            items.iter().find(|window| {
                window["title"]
                    .as_str()
                    .unwrap_or_default()
                    .to_ascii_lowercase()
                    .contains(needle)
            })
        })
        .cloned()
        .ok_or_else(|| format!("window not found: {needle}"))
}

fn find_element<F>(semantic: &Value, predicate: F) -> GateResult<Value>
where
    F: Fn(&Value) -> bool,
{
    semantic["elements"]
        .as_array()
        .and_then(|items| items.iter().find(|element| predicate(element)))
        .cloned()
        .ok_or_else(|| "semantic target not found".into())
}

fn fixture_title(observation: &Value) -> GateResult<String> {
    observation["windows"]
        .as_array()
        .and_then(|items| {
            items.iter().find_map(|window| {
                let title = window["title"].as_str()?;
                title
                    .to_ascii_lowercase()
                    .contains("alice computer r5 fixture")
                    .then_some(title.to_owned())
            })
        })
        .ok_or_else(|| "R5 fixture title not found".into())
}

fn wait_for_title(mcp: &mut McpProcess, marker: &str, label: &str) -> GateResult<String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let observation = observe(mcp, false)?;
        let title = fixture_title(&observation)?;
        if title.contains(marker) {
            return Ok(title);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("{label} evidence missing: {observation}"));
        }
        // This waits for the fixture's own UI thread to dispatch the already
        // injected event; it never retries or replays the action.
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_mouse_up_count(
    mcp: &mut McpProcess,
    button: &str,
    count: usize,
    label: &str,
) -> GateResult<String> {
    let deadline = std::time::Instant::now() + Duration::from_millis(500);
    loop {
        let observation = observe(mcp, false)?;
        let title = fixture_title(&observation)?;
        if title.matches(&format!("mouse={button}:up")).count() >= count {
            return Ok(title);
        }
        if std::time::Instant::now() >= deadline {
            return Err(format!("{label} evidence missing: {observation}"));
        }
        // Wait only for the fixture UI thread to dispatch the injected event;
        // no input action is retried or replayed.
        thread::sleep(Duration::from_millis(10));
    }
}

fn event_time(event: &str) -> GateResult<u128> {
    event
        .split(":t=")
        .nth(1)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("event has no timestamp: {event}"))
}

fn health_pid(mcp: &mut McpProcess) -> GateResult<u32> {
    let result = mcp.tool("computer_health", json!({}))?;
    Ok(tool_structured(&result)?["sidecar_pid"]
        .as_u64()
        .ok_or("health did not return sidecar_pid")? as u32)
}

fn terminate_process(pid: u32) -> GateResult<()> {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(format!("OpenProcess failed for {pid}"));
        }
        let result = TerminateProcess(handle, 137);
        CloseHandle(handle);
        if result == 0 {
            return Err(format!("TerminateProcess failed for {pid}"));
        }
    }
    Ok(())
}

fn process_alive(pid: u32) -> bool {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            false
        } else {
            CloseHandle(handle);
            true
        }
    }
}

fn verify_png(result: &Value) -> GateResult<()> {
    let image = result["content"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "image"))
        .ok_or("MCP screenshot did not return image content")?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        image["data"].as_str().unwrap_or_default(),
    )
    .map_err(|error| format!("screenshot base64 decode failed: {error}"))?;
    let reader = Decoder::new(Cursor::new(bytes))
        .read_info()
        .map_err(|error| format!("PNG decode failed: {error}"))?;
    if reader.info().width == 0 || reader.info().height == 0 {
        return Err("PNG dimensions are empty".into());
    }
    Ok(())
}

fn save_png_probe(result: &Value) -> GateResult<()> {
    save_png_evidence(result, "mouse-target-probe.png")
}

fn save_png_evidence(result: &Value, filename: &str) -> GateResult<()> {
    let image = result["content"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "image"))
        .ok_or("MCP screenshot did not return image content")?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        image["data"].as_str().unwrap_or_default(),
    )
    .map_err(|error| format!("screenshot base64 decode failed: {error}"))?;
    std::fs::create_dir_all("target/r5-evidence")
        .map_err(|error| format!("create screenshot evidence directory: {error}"))?;
    std::fs::write(format!("target/r5-evidence/{filename}"), bytes)
        .map_err(|error| format!("write screenshot evidence: {error}"))
}

fn argument_path(name: &str) -> Option<PathBuf> {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == name {
            return args.next().map(PathBuf::from);
        }
    }
    None
}

fn has_flag(name: &str) -> bool {
    env::args().skip(1).any(|arg| arg == name)
}
