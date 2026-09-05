//! Computer-INFRA-R6 MCP regression for the unchanged five-tool surface.
//!
//! This is a harness only. It starts the production MCP adapter, which in turn
//! uses the production sidecar/client and WinNative backend.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    env,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

type GateResult<T> = Result<T, Box<dyn std::error::Error>>;

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn spawn(adapter: &PathBuf, sidecar: &PathBuf) -> GateResult<Self> {
        let mut child = Command::new(adapter)
            .arg("--sidecar")
            .arg(sidecar)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(Self {
            stdin: child.stdin.take().ok_or("MCP stdin missing")?,
            stdout: BufReader::new(child.stdout.take().ok_or("MCP stdout missing")?),
            child,
            next_id: 1,
        })
    }

    fn notify(&mut self, method: &str, params: Value) -> GateResult<()> {
        self.write(&json!({"jsonrpc":"2.0","method":method,"params":params}))
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
        self.stdout.read_line(&mut line)?;
        if line.is_empty() {
            return Err(format!("MCP EOF while waiting for {method}").into());
        }
        let response: Value = serde_json::from_str(&line)?;
        if response["id"] != id {
            return Err(format!("MCP response id mismatch: {response}").into());
        }
        if response.get("error").is_some() {
            return Err(format!("MCP JSON-RPC error: {response}").into());
        }
        Ok(response["result"].clone())
    }

    fn write(&mut self, value: &Value) -> GateResult<()> {
        serde_json::to_writer(&mut self.stdin, value)?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        Ok(())
    }

    fn tool(&mut self, name: &str, arguments: Value) -> GateResult<Value> {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
    }

    fn close(mut self) -> GateResult<()> {
        drop(self.stdin);
        let status = self.child.wait()?;
        if !status.success() {
            return Err(format!("MCP adapter exited with {status}").into());
        }
        Ok(())
    }
}

fn main() -> GateResult<()> {
    let adapter = env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--adapter=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer-mcp.exe"));
    let sidecar = env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--sidecar=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    println!("runner=alice-computer-mcp/r6_mcp_regression");
    let mut mcp = Mcp::spawn(&adapter, &sidecar)?;
    let initialize = mcp.call("initialize", json!({}))?;
    if initialize["serverInfo"]["name"] != "alice-computer-mcp" {
        return Err("unexpected MCP server info".into());
    }
    mcp.notify("notifications/initialized", json!({}))?;

    let tools = mcp.call("tools/list", json!({}))?;
    let names = tools["tools"]
        .as_array()
        .ok_or("tools/list missing tools")?
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect::<Vec<_>>();
    let expected = vec![
        "computer_health",
        "computer_observe",
        "computer_execute",
        "computer_use",
        "computer_screenshot",
        "computer_validate",
    ];
    if names != expected {
        return Err(format!("MCP tool surface changed: {names:?}").into());
    }
    println!("gate7.tools=PASS count=6 names={names:?}");

    let health = mcp.tool("computer_health", json!({}))?;
    if health["isError"] == true || health["structuredContent"]["ready"] != true {
        return Err(format!("computer_health failed: {health}").into());
    }
    let observation = mcp.tool("computer_observe", json!({"screenshot_metadata": true}))?;
    let structured = observation
        .get("structuredContent")
        .ok_or("computer_observe missing structuredContent")?;
    if structured["display_topology"]["displays"]
        .as_array()
        .is_none()
        || structured["screenshot"]["coordinate_space"] != "screenshot_pixel"
    {
        return Err(format!("R6 display metadata missing from MCP observe: {structured}").into());
    }
    println!(
        "gate7.observe=PASS displays={} screenshot_space={} topology_generation={}",
        structured["display_topology"]["displays"]
            .as_array()
            .map_or(0, Vec::len),
        structured["screenshot"]["coordinate_space"],
        structured["display_topology"]["topology_generation"]
    );

    let screenshot = mcp.tool("computer_screenshot", json!({}))?;
    let metadata = screenshot
        .get("structuredContent")
        .and_then(|value| value.get("metadata"))
        .ok_or("computer_screenshot missing metadata")?;
    let image = screenshot["content"]
        .as_array()
        .and_then(|content| content.iter().find(|item| item["type"] == "image"))
        .and_then(|item| item["data"].as_str())
        .ok_or("computer_screenshot missing image content")?;
    let bytes = STANDARD.decode(image)?;
    if bytes.is_empty() || metadata["coordinate_space"] != "screenshot_pixel" {
        return Err("MCP screenshot evidence invalid".into());
    }
    println!(
        "gate7.screenshot=PASS width={} height={} png_bytes={} frame_id={}",
        metadata["width"],
        metadata["height"],
        bytes.len(),
        metadata["frame_id"]
    );

    let health_again = mcp.tool("computer_health", json!({}))?;
    if health_again["structuredContent"]["ready"] != true {
        return Err("MCP sidecar was not ready after display calls".into());
    }
    println!("gate7.regression=PASS response_correlation=true stdout_clean=true");
    mcp.close()?;
    println!("r6_mcp_result=PASS");
    Ok(())
}
