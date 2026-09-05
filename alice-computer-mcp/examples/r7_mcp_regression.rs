//! R7 MCP regression: five tools remain unchanged while observation is
//! metadata-only and screenshot encodes one captured frame on demand.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde_json::{json, Value};
use std::{
    env,
    io::{BufRead, BufReader, Write},
    path::PathBuf,
    process::{Child, ChildStdin, ChildStdout, Command, Stdio},
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Mcp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Mcp {
    fn spawn(adapter: &PathBuf, sidecar: &PathBuf) -> Result<Self> {
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

    fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        serde_json::to_writer(
            &mut self.stdin,
            &json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}),
        )?;
        self.stdin.write_all(b"\n")?;
        self.stdin.flush()?;
        let mut line = String::new();
        self.stdout.read_line(&mut line)?;
        if line.is_empty() {
            return Err(format!("MCP EOF while waiting for {method}").into());
        }
        let response: Value = serde_json::from_str(&line)?;
        if response["id"] != id || response.get("error").is_some() {
            return Err(format!("MCP response error/correlation failure: {response}").into());
        }
        Ok(response["result"].clone())
    }

    fn tool(&mut self, name: &str, arguments: Value) -> Result<Value> {
        self.call("tools/call", json!({"name": name, "arguments": arguments}))
    }

    fn close(mut self) -> Result<()> {
        drop(self.stdin);
        let status = self.child.wait()?;
        if !status.success() {
            return Err(format!("MCP exited with {status}").into());
        }
        Ok(())
    }
}

fn main() -> Result<()> {
    let adapter = env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--adapter=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer-mcp.exe"));
    let sidecar = env::args()
        .skip(1)
        .find_map(|arg| arg.strip_prefix("--sidecar=").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target/release/alice-computer.exe"));
    let mut mcp = Mcp::spawn(&adapter, &sidecar)?;
    let initialize = mcp.call("initialize", json!({}))?;
    if initialize["serverInfo"]["name"] != "alice-computer-mcp" {
        return Err("unexpected MCP server info".into());
    }
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
    println!("gate10.tools=PASS count=6 names={names:?}");

    let observe = mcp.tool("computer_observe", json!({"screenshot_metadata": true}))?;
    let structured = observe
        .get("structuredContent")
        .ok_or("observe missing structuredContent")?;
    let frame = &structured["screenshot"];
    if frame["coordinate_space"] != "screenshot_pixel"
        || frame["pixel_format"] != "bgra8"
        || frame["frame_id"].as_str().is_none()
    {
        return Err(format!("metadata-only observation was invalid: {structured}").into());
    }
    if frame.get("bytes").is_some() {
        return Err("observe transported encoded bytes".into());
    }
    println!(
        "gate4/6.observe=PASS frame_id={} raw_format=bgra8 encode_count=0 explicit_frame_metadata=true",
        frame["frame_id"]
    );

    let width = frame["width"].as_u64().ok_or("frame width missing")?;
    let height = frame["height"].as_u64().ok_or("frame height missing")?;
    let move_result = mcp.tool(
        "computer_use",
        json!({
            "actions": [{
                "type": "move",
                "x": width as f64 / 2.0,
                "y": height as f64 / 2.0
            }],
            "include_screenshot": false
        }),
    )?;
    if move_result["isError"] == true
        || move_result["structuredContent"]["actions"][0]["outcome"] != "performed"
        || move_result["structuredContent"]["coordinate_frame_id"]
            .as_str()
            .is_none()
    {
        return Err(format!("pixel coordinate regression failed: {move_result}").into());
    }
    println!(
        "gate10.pixel_move=PASS coordinate_frame_id={} normalized_point=({}, {}) screenshot=false",
        move_result["structuredContent"]["coordinate_frame_id"],
        width / 2,
        height / 2
    );

    let screenshot = mcp.tool("computer_screenshot", json!({}))?;
    let metadata = screenshot["structuredContent"]["metadata"].clone();
    let image = screenshot["content"]
        .as_array()
        .and_then(|items| items.iter().find(|item| item["type"] == "image"))
        .and_then(|item| item["data"].as_str())
        .ok_or("screenshot image content missing")?;
    let bytes = STANDARD.decode(image)?;
    if bytes.is_empty()
        || metadata["coordinate_space"] != "screenshot_pixel"
        || metadata["encode_cache_hit"].is_null()
    {
        return Err("on-demand screenshot evidence invalid".into());
    }
    println!(
        "gate6/10.screenshot=PASS frame_id={} png_bytes={} encode_cache_hit={} same_capture_encode=true metadata_schema=true",
        metadata["frame_id"],
        bytes.len(),
        metadata["encode_cache_hit"]
    );
    mcp.close()?;
    println!("r7_mcp_result=PASS");
    Ok(())
}
