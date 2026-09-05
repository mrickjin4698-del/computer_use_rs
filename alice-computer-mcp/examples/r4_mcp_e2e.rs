//! Real MCP stdio harness for Computer-INFRA-R4.
//!
//! This is a test harness, not production MCP behavior.  It starts the real
//! `alice-computer-mcp` binary, speaks MCP JSON-RPC over stdio, and verifies
//! the sidecar-backed execution policy on the interactive Windows desktop.

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

type HarnessResult<T> = Result<T, String>;

struct McpProcess {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    fn spawn(adapter: &PathBuf, sidecar: &PathBuf) -> HarnessResult<Self> {
        Self::spawn_with_timeout(adapter, sidecar, None)
    }

    fn spawn_with_timeout(
        adapter: &PathBuf,
        sidecar: &PathBuf,
        timeout_ms: Option<u64>,
    ) -> HarnessResult<Self> {
        let mut command = Command::new(adapter);
        command
            .arg("--sidecar")
            .arg(sidecar)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(timeout_ms) = timeout_ms {
            command.env("ALICE_COMPUTER_MCP_TIMEOUT_MS", timeout_ms.to_string());
        }
        let mut child = command
            .spawn()
            .map_err(|error| format!("spawn MCP adapter: {error}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "MCP stdin was not piped".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "MCP stdout was not piped".to_owned())?;
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                while reader
                    .read_line(&mut line)
                    .ok()
                    .filter(|count| *count > 0)
                    .is_some()
                {
                    line.clear();
                }
            });
        }
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
            next_id: 1,
        })
    }

    fn initialize(&mut self) -> HarnessResult<()> {
        let result = self.call("initialize", json!({}))?;
        if result["serverInfo"]["name"] != "alice-computer-mcp" {
            return Err("MCP initialize returned unexpected serverInfo".into());
        }
        self.notify("notifications/initialized", json!({}))?;
        Ok(())
    }

    fn notify(&mut self, method: &str, params: Value) -> HarnessResult<()> {
        let request = json!({ "jsonrpc": "2.0", "method": method, "params": params });
        write_json(&mut self.stdin, &request)
    }

    fn call(&mut self, method: &str, params: Value) -> HarnessResult<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        write_json(&mut self.stdin, &request)?;
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|error| format!("read MCP response for {method}: {error}"))?;
        if line.is_empty() {
            return Err(format!("MCP EOF while waiting for {method}"));
        }
        let response: Value = serde_json::from_str(&line)
            .map_err(|error| format!("MCP stdout was not JSON for {method}: {error}"))?;
        if response["id"] != id {
            return Err(format!("MCP response correlation mismatch for {method}"));
        }
        if response.get("error").is_some() {
            return Err(format!("MCP JSON-RPC error for {method}: {response}"));
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| format!("MCP response had no result for {method}"))
    }

    fn tool(&mut self, name: &str, arguments: Value) -> HarnessResult<Value> {
        self.call(
            "tools/call",
            json!({ "name": name, "arguments": arguments }),
        )
    }

    fn close(mut self) -> HarnessResult<()> {
        drop(self.stdin);
        let status = self
            .child
            .wait()
            .map_err(|error| format!("wait MCP adapter: {error}"))?;
        if !status.success() {
            return Err(format!("MCP adapter exited with {status}"));
        }
        Ok(())
    }

    fn kill(&mut self) -> HarnessResult<()> {
        let _ = self.child.kill();
        let _ = self.child.wait();
        Ok(())
    }
}

fn main() -> HarnessResult<()> {
    let adapter = argument_path("--adapter")
        .unwrap_or_else(|| PathBuf::from("target/debug/alice-computer-mcp.exe"));
    let sidecar = argument_path("--sidecar")
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    let fixture_path = argument_path("--fixture")
        .unwrap_or_else(|| PathBuf::from("target/debug/examples/r0_input_fixture.exe"));
    let start_fixture = has_flag("--start-fixture");
    let mut fixture_child = if start_fixture {
        let child = Command::new(&fixture_path)
            .env("ALICE_R3_SLOW_INVOKE", "1")
            .stdin(Stdio::null())
            .spawn()
            .map_err(|error| format!("start fixture: {error}"))?;
        thread::sleep(Duration::from_millis(300));
        Some(child)
    } else {
        None
    };

    println!(
        "r4 adapter={} sidecar={}",
        adapter.display(),
        sidecar.display()
    );
    let mut mcp = McpProcess::spawn(&adapter, &sidecar)?;
    mcp.initialize()?;
    let tools = mcp.call("tools/list", json!({}))?;
    gate1_tools(&tools)?;
    println!("gate1_discovery=PASS");

    let mut observation = observe(&mut mcp, true)?;
    let fixture = find_window(&observation, "alice computer input fixture")?;
    let notepad = find_window(&observation, "notepad")?;
    let notepad_semantic = semantic_observe(&mut mcp, &notepad["id"])?;
    if notepad_semantic["elements"].as_array().is_none() {
        return Err("Notepad semantic observation was not structured".into());
    }
    let semantic = semantic_observe(&mut mcp, &fixture["id"])?;
    if semantic["elements"].as_array().is_none()
        || semantic["metadata"]["generation"].as_u64().unwrap_or(0) == 0
    {
        return Err("semantic observation did not return elements and generation".into());
    }
    println!("gate2_observe=PASS");

    let button = find_element(&semantic, |element| {
        element["name"] == "Set Status" && element["capabilities"]["invokable"] == true
    })?;
    let button_before = fixture_marker(&observation, "BUTTON_INVOKES=")?;
    let execute = execute_semantic(&mut mcp, "invoke", &button["id"], None, "deny")?;
    let observation_after = observe(&mut mcp, false)?;
    let button_after = fixture_marker(&observation_after, "BUTTON_INVOKES=")?;
    if execute["isError"] == true
        || execute["structuredContent"]["final_outcome"] != "performed"
        || execute["structuredContent"]["fallback_used"] != false
        || execute["structuredContent"]["verification"]["verified"] != true
        || button_after != button_before + 1
    {
        return Err(format!("semantic execute gate failed: {execute}"));
    }
    println!("gate3_semantic_execute=PASS");

    observation = observe(&mut mcp, true)?;
    let pixel_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
    let pixel_target = find_element(&pixel_semantic, |element| {
        element["name"] == "Pixel Only Target"
            && element["capabilities"]["invokable"] == false
            && element["bounds"].is_object()
    })?;
    let pixel_before = fixture_marker(&observation, "PIXEL_CLICKS=")?;
    let fallback = execute_semantic(&mut mcp, "invoke", &pixel_target["id"], None, "allow")?;
    let fallback_observation = observe(&mut mcp, false)?;
    let pixel_after = fixture_marker(&fallback_observation, "PIXEL_CLICKS=")?;
    if fallback["isError"] == true
        || fallback["structuredContent"]["final_outcome"] != "performed"
        || fallback["structuredContent"]["fallback_used"] != true
        || fallback["structuredContent"]["attempts"]
            .as_array()
            .is_none_or(|items| items.len() != 2)
        || pixel_after != pixel_before + 1
    {
        return Err(format!("pixel fallback gate failed: {fallback}"));
    }
    let denied_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
    let denied_target = find_element(&denied_semantic, |element| {
        element["name"] == "Pixel Only Target" && element["bounds"].is_object()
    })?;
    let denied = execute_semantic(&mut mcp, "invoke", &denied_target["id"], None, "deny")?;
    let denied_after = fixture_marker(&observe(&mut mcp, false)?, "PIXEL_CLICKS=")?;
    if denied["structuredContent"]["fallback_used"] != false
        || denied["structuredContent"]["attempts"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item["method"] == "pixel_click"))
        || denied_after != pixel_after
    {
        return Err(format!("fallback denial gate failed: {denied}"));
    }
    println!("gate4_pixel_fallback=PASS");

    let exact_values = ["ALICE_MCP_R4_123", "_-+=", "中文_R4"];
    for expected in exact_values {
        let _current = observe(&mut mcp, false)?;
        let current_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
        let edit = find_element(&current_semantic, |element| {
            element["capabilities"]["editable"] == true
                && (element["control_type"] == "edit" || element["control_type"] == "document")
        })
        .map_err(|error| format!("SetValue edit target for {expected}: {error}"))?;
        let set_value =
            execute_semantic(&mut mcp, "set_value", &edit["id"], Some(expected), "deny")?;
        if set_value["isError"] == true
            || set_value["structuredContent"]["final_outcome"] != "performed"
            || set_value["structuredContent"]["verification"]["verified"] != true
        {
            return Err(format!("SetValue failed for {expected}: {set_value}"));
        }
        let _verified = observe(&mut mcp, false)?;
        let verified_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
        let actual = find_element(&verified_semantic, |element| {
            element["capabilities"]["editable"] == true
                && (element["control_type"] == "edit" || element["control_type"] == "document")
        })
        .map_err(|error| format!("SetValue re-observation for {expected}: {error}"))?;
        let actual = actual["value_summary"]
            .as_str()
            .or_else(|| actual["text_summary"].as_str())
            .unwrap_or_default();
        if actual != expected {
            return Err(format!(
                "SetValue exact mismatch: expected {expected:?}, actual {actual:?}"
            ));
        }
    }
    println!("gate5_set_value=PASS");

    let _stale_observation = observe(&mut mcp, false)?;
    let stale_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
    let stale_button = find_element(&stale_semantic, |element| {
        element["name"] == "Set Status" && element["capabilities"]["invokable"] == true
    })?;
    let _refresh = semantic_observe(&mut mcp, &fixture["id"])?;
    let stale = execute_semantic(&mut mcp, "invoke", &stale_button["id"], None, "allow")?;
    if stale["structuredContent"]["final_outcome"] != "stale_element"
        || stale["structuredContent"]["attempts"]
            .as_array()
            .is_none_or(|items| items.iter().any(|item| item["method"] == "pixel_click"))
    {
        return Err(format!("stale safety gate failed: {stale}"));
    }
    println!("gate6_stale_safety=PASS");

    let uncertain_observation = observe(&mut mcp, false)?;
    let uncertain_semantic = semantic_observe(&mut mcp, &fixture["id"])?;
    let uncertain_button = find_element(&uncertain_semantic, |element| {
        element["name"] == "Set Status" && element["capabilities"]["invokable"] == true
    })?;
    let before_uncertain = fixture_marker(&uncertain_observation, "BUTTON_INVOKES=")?;
    let sidecar_pid = health_pid(&mut mcp)?;
    let kill = thread::spawn(move || {
        thread::sleep(Duration::from_millis(250));
        terminate_process(sidecar_pid)
    });
    let uncertain = execute_semantic(&mut mcp, "invoke", &uncertain_button["id"], None, "allow")?;
    let _ = kill.join();
    let unknown = uncertain["isError"] == true
        && uncertain["structuredContent"]["error"]["outcome_unknown"] == true;
    if !unknown {
        return Err(format!("OutcomeUnknown gate failed: {uncertain}"));
    }
    mcp.kill()?;
    let mut restarted = McpProcess::spawn(&adapter, &sidecar)?;
    restarted.initialize()?;
    let restarted_observation = observe(&mut restarted, false)?;
    let after_uncertain = fixture_marker(&restarted_observation, "BUTTON_INVOKES=")?;
    if after_uncertain > before_uncertain + 1 {
        return Err(format!(
            "possible replay after restart: before={before_uncertain}, after={after_uncertain}"
        ));
    }
    println!("gate7_outcome_unknown=PASS");

    let stable_adapter_pid = restarted.child.id();
    let stable_sidecar_pid = health_pid(&mut restarted)?;
    for index in 0..100 {
        if index % 3 == 0 {
            let _ = restarted.tool("computer_health", json!({}))?;
        } else if index % 3 == 1 {
            let _ = restarted.tool("computer_observe", json!({}))?;
        } else {
            let window = find_window(
                &observe(&mut restarted, false)?,
                "alice computer input fixture",
            )?;
            let result =
                restarted.tool("computer_validate", json!({ "window_id": window["id"] }))?;
            assert_tool_ok(&result, "lifecycle validate")?;
        }
    }
    if restarted.child.id() != stable_adapter_pid
        || health_pid(&mut restarted)? != stable_sidecar_pid
    {
        return Err("MCP or sidecar PID changed during lifecycle gate".into());
    }
    println!("gate8_lifecycle=PASS");

    let malformed = restarted.tool("computer_execute", json!({ "intent": "bad" }))?;
    if malformed["isError"] != true
        || malformed["structuredContent"]["error"]["code"] != "INVALID_ARGUMENTS"
    {
        return Err(format!(
            "malformed argument was not a structured MCP error: {malformed}"
        ));
    }
    let unknown_element = execute_semantic(
        &mut restarted,
        "invoke",
        &json!("unknown-element"),
        None,
        "deny",
    )?;
    if unknown_element["isError"] != true
        || unknown_element["structuredContent"]["final_outcome"] != "unknown_element"
    {
        return Err(format!(
            "unknown element was not a structured MCP error: {unknown_element}"
        ));
    }
    let mut timeout_mcp = McpProcess::spawn_with_timeout(&adapter, &sidecar, Some(100))?;
    timeout_mcp.initialize()?;
    let timeout_result = timeout_mcp.tool("computer_screenshot", json!({}))?;
    if timeout_result["isError"] != true
        || timeout_result["structuredContent"]["error"]["code"] != "TIMEOUT"
    {
        return Err(format!(
            "RPC timeout was not a structured MCP error: {timeout_result}"
        ));
    }
    timeout_mcp.close()?;
    let mismatch_sidecar = argument_path("--mismatch-sidecar")
        .unwrap_or_else(|| PathBuf::from("target/debug/examples/r4_mismatch_sidecar.exe"));
    let mut mismatch = McpProcess::spawn(&adapter, &mismatch_sidecar)?;
    if mismatch.initialize().is_ok() {
        return Err("protocol mismatch sidecar unexpectedly initialized".into());
    }
    mismatch.kill()?;
    let screenshot = restarted.tool("computer_screenshot", json!({}))?;
    verify_png(&screenshot)?;
    println!("gate9_protocol_failure=PASS");

    restarted.close()?;
    if process_alive(stable_sidecar_pid) {
        return Err(format!(
            "sidecar residual after MCP shutdown: {stable_sidecar_pid}"
        ));
    }
    println!("shutdown=PASS residual_sidecar=0");
    if let Some(mut fixture) = fixture_child.take() {
        let _ = fixture.kill();
        let _ = fixture.wait();
    }
    println!("result=PASS");
    Ok(())
}

fn write_json(writer: &mut ChildStdin, value: &Value) -> HarnessResult<()> {
    serde_json::to_writer(&mut *writer, value).map_err(|error| error.to_string())?;
    writer.write_all(b"\n").map_err(|error| error.to_string())?;
    writer.flush().map_err(|error| error.to_string())
}

fn tool_structured(result: &Value) -> HarnessResult<&Value> {
    if result["isError"] == true {
        return Err(format!("tool returned error: {result}"));
    }
    result
        .get("structuredContent")
        .ok_or_else(|| format!("tool result has no structuredContent: {result}"))
}

fn assert_tool_ok(result: &Value, label: &str) -> HarnessResult<()> {
    if result["isError"] == true {
        return Err(format!("{label} failed: {result}"));
    }
    Ok(())
}

fn observe(mcp: &mut McpProcess, screenshot_metadata: bool) -> HarnessResult<Value> {
    let result = mcp.tool(
        "computer_observe",
        json!({ "screenshot_metadata": screenshot_metadata }),
    )?;
    Ok(tool_structured(&result)?.clone())
}

fn semantic_observe(mcp: &mut McpProcess, window_id: &Value) -> HarnessResult<Value> {
    let result = mcp.tool(
        "computer_observe",
        json!({ "window_id": window_id, "semantic": true, "max_depth": 8, "max_elements": 512 }),
    )?;
    Ok(tool_structured(&result)?["semantic"].clone())
}

fn execute_semantic(
    mcp: &mut McpProcess,
    operation: &str,
    element_id: &Value,
    value: Option<&str>,
    fallback_policy: &str,
) -> HarnessResult<Value> {
    let mut intent = json!({ "operation": operation, "element_id": element_id });
    if let Some(value) = value {
        intent["value"] = json!(value);
    }
    mcp.tool(
        "computer_execute",
        json!({ "intent": intent, "strategy": "prefer_semantic", "fallback_policy": fallback_policy }),
    )
}

fn find_window(observation: &Value, needle: &str) -> HarnessResult<Value> {
    observation["windows"]
        .as_array()
        .and_then(|windows| {
            windows.iter().find(|window| {
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

fn find_element<F>(semantic: &Value, predicate: F) -> HarnessResult<Value>
where
    F: Fn(&Value) -> bool,
{
    semantic["elements"]
        .as_array()
        .and_then(|elements| elements.iter().find(|element| predicate(element)))
        .cloned()
        .ok_or_else(|| "semantic target not found".into())
}

fn fixture_marker(observation: &Value, marker: &str) -> HarnessResult<u32> {
    let titles = observation["windows"]
        .as_array()
        .map(|windows| {
            windows
                .iter()
                .filter_map(|window| window["title"].as_str())
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let title = titles
        .iter()
        .copied()
        .find(|title| {
            title
                .to_ascii_lowercase()
                .contains("alice computer input fixture")
        })
        .ok_or_else(|| format!("fixture title not found; titles={titles:?}"))?;
    title
        .split(marker)
        .nth(1)
        .and_then(|value| value.split(';').next())
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| format!("fixture marker not found: {marker}; title={title:?}"))
}

fn health_pid(mcp: &mut McpProcess) -> HarnessResult<u32> {
    let result = mcp.tool("computer_health", json!({}))?;
    Ok(tool_structured(&result)?["sidecar_pid"]
        .as_u64()
        .ok_or_else(|| "health did not return sidecar_pid".to_owned())? as u32)
}

fn verify_png(result: &Value) -> HarnessResult<()> {
    let image = result["content"]
        .as_array()
        .and_then(|content| content.iter().find(|item| item["type"] == "image"))
        .ok_or_else(|| "screenshot did not return MCP image content".to_owned())?;
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        image["data"].as_str().unwrap_or_default(),
    )
    .map_err(|error| format!("screenshot base64 decode failed: {error}"))?;
    let decoder = Decoder::new(Cursor::new(bytes));
    let reader = decoder
        .read_info()
        .map_err(|error| format!("PNG decode failed: {error}"))?;
    if reader.info().width == 0 || reader.info().height == 0 {
        return Err("PNG dimensions were empty".into());
    }
    Ok(())
}

fn gate1_tools(tools: &Value) -> HarnessResult<()> {
    let entries = tools["tools"]
        .as_array()
        .ok_or_else(|| "tools/list did not return tools".to_owned())?;
    let names = entries
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    let expected = [
        "computer_observe",
        "computer_execute",
        "computer_use",
        "computer_screenshot",
        "computer_validate",
        "computer_health",
    ];
    if names.len() != expected.len() || expected.iter().any(|name| !names.contains(name)) {
        return Err(format!("unexpected MCP tool list: {names:?}"));
    }
    let schema_bytes: usize = entries
        .iter()
        .map(|tool| {
            serde_json::to_vec(tool)
                .map(|bytes| bytes.len())
                .unwrap_or(0)
        })
        .sum();
    println!("schema tools={} total_bytes={schema_bytes}", names.len());
    if schema_bytes > 16_384 {
        return Err(format!("MCP schema budget exceeded: {schema_bytes} bytes"));
    }
    for tool in entries {
        let serialized = serde_json::to_string(tool).unwrap_or_default();
        for forbidden in ["HWND", "IUIAutomation", "RuntimeId", "raw_uia", "raw_click"] {
            if serialized.contains(forbidden) {
                return Err(format!(
                    "forbidden implementation detail in schema: {forbidden}"
                ));
            }
        }
    }
    Ok(())
}

fn terminate_process(pid: u32) -> HarnessResult<()> {
    unsafe {
        let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if handle.is_null() {
            return Err(format!("OpenProcess failed for {pid}"));
        }
        let ok = TerminateProcess(handle, 137);
        CloseHandle(handle);
        if ok == 0 {
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
