//! Negative-only sidecar stub for the R4 protocol-mismatch gate.
//! It is never used by the production MCP adapter.

#![cfg(windows)]

use serde_json::{json, Value};
use std::io::{self, Read, Write};

fn main() {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut input = stdin.lock();
    let mut output = stdout.lock();
    loop {
        let mut header = [0u8; 4];
        if input.read_exact(&mut header).is_err() {
            return;
        }
        let length = u32::from_le_bytes(header) as usize;
        let mut payload = vec![0u8; length];
        if input.read_exact(&mut payload).is_err() {
            return;
        }
        let request: Value = match serde_json::from_slice(&payload) {
            Ok(value) => value,
            Err(_) => return,
        };
        let method = request["method"].as_str().unwrap_or_default();
        let response = match method {
            "hello" => json!({
                "request_id": request["request_id"],
                "ok": true,
                "result": {
                    "protocol_version": 999,
                    "sidecar_version": "mismatch-test",
                    "backend": "wrong_backend",
                    "capabilities": [],
                    "pid": std::process::id()
                }
            }),
            "shutdown" => {
                let response = json!({
                    "request_id": request["request_id"],
                    "ok": true,
                    "result": { "closed_sessions": 0, "stopped": true }
                });
                write_frame(&mut output, &response);
                return;
            }
            _ => json!({
                "request_id": request["request_id"],
                "ok": false,
                "error": {
                    "code": "UNKNOWN_METHOD",
                    "message": "mismatch stub",
                    "retryable": false,
                    "outcome_unknown": false
                }
            }),
        };
        write_frame(&mut output, &response);
    }
}

fn write_frame<W: Write>(output: &mut W, value: &Value) {
    let payload = serde_json::to_vec(value).expect("stub response serializes");
    let _ = output.write_all(&(payload.len() as u32).to_le_bytes());
    let _ = output.write_all(&payload);
    let _ = output.flush();
}
