//! Standalone MCP adapter for the Alice Computer sidecar.
//!
//! This process owns one Host Broker and one broker session. It exposes only
//! policy-shaped computer tools over MCP stdio. Native runtime, WinNative,
//! UIA, and Windows implementation details stay behind the sidecar boundary.

use alice_computer_use_broker::{
    BrokerError, BrokerExecutionRequest, BrokerSessionRef, ComputerExecutionBroker,
    ComputerRequestOwner,
};
use alice_computer_use_core::{
    ComputerAction, ComputerExecutionIntent, ComputerExecutionOutcome, ComputerExecutionRequest,
    ComputerExecutionStrategy, ComputerFallbackPolicy, Coordinate, ElementId, FrameEncoding,
    MouseButton, SemanticAction, SemanticObservationLimits, WindowId,
};
use alice_computer_use_sidecar_client::{ComputerHostService, ComputerHostServiceConfig};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::{
    env,
    io::{self, BufRead, Write},
    path::PathBuf,
    process,
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "alice-computer-mcp";
const SERVER_VERSION: &str = "0.1.0";

type AdapterResult<T> = Result<T, String>;

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    #[serde(default)]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

struct McpServer {
    broker: ComputerExecutionBroker,
    session: BrokerSessionRef,
    next_request: AtomicU64,
}

impl McpServer {
    fn new(sidecar_path: PathBuf) -> AdapterResult<Self> {
        let timeout = env::var("ALICE_COMPUTER_MCP_TIMEOUT_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map(Duration::from_millis)
            .unwrap_or_else(|| Duration::from_secs(30));
        let host = ComputerHostService::new(ComputerHostServiceConfig::new(sidecar_path, timeout));
        let broker = ComputerExecutionBroker::new(host);
        let owner = ComputerRequestOwner::new(
            "mcp-process",
            "mcp-process",
            "mcp-session",
            "mcp-session",
            None,
        );
        let session = broker
            .open_session(owner)
            .map_err(|error| format_broker_error(&error))?;
        Ok(Self {
            broker,
            session,
            next_request: AtomicU64::new(1),
        })
    }

    fn handle(&self, request: JsonRpcRequest) -> Option<Value> {
        let id = request.id?;
        let result = match request.method.as_str() {
            "initialize" => Ok(initialize_result()),
            "notifications/initialized" => Ok(json!({})),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => self.handle_tool_call(request.params.unwrap_or_else(|| json!({}))),
            _ => Err(json_rpc_error(
                -32601,
                format!("method not found: {}", request.method),
            )),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        })
    }

    fn handle_tool_call(&self, params: Value) -> Result<Value, Value> {
        let object = params.as_object().ok_or_else(|| {
            tool_error("INVALID_ARGUMENTS", "tools/call params must be an object")
        })?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", "tools/call requires a tool name"))?;
        let arguments = object
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        match name {
            "computer_health" => Ok(self.health()),
            "computer_observe" => Ok(self.observe(arguments)),
            "computer_execute" => Ok(self.execute(arguments)),
            "computer_screenshot" => Ok(self.screenshot(arguments)),
            "computer_validate" => Ok(self.validate(arguments)),
            _ => Err(tool_error(
                "UNKNOWN_TOOL",
                format!("unknown computer tool: {name}"),
            )),
        }
    }

    fn health(&self) -> Value {
        match self.broker.health() {
            Ok(health) => tool_success(
                json!({
                    "ready": health.ready,
                    "initialized": health.initialized,
                    "session_count": health.session_count,
                    "sidecar_pid": health.pid,
                    "session_open": true,
                    "security": health.security,
                    "desktop": health.desktop,
                }),
                "computer sidecar is healthy",
            ),
            Err(error) => tool_broker_error(&error),
        }
    }

    fn observe(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let window_id = match args.optional_string("window_id") {
            Ok(value) => value.map(WindowId::new),
            Err(error) => return error,
        };
        let semantic = match args.boolean("semantic", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let screenshot_metadata = match args.boolean("screenshot_metadata", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let capabilities = match args.boolean("capabilities", false) {
            Ok(value) => value,
            Err(error) => return error,
        };
        let limits = match args.limits() {
            Ok(value) => value,
            Err(error) => return error,
        };
        if semantic && window_id.is_none() {
            return tool_error(
                "INVALID_ARGUMENTS",
                "semantic=true requires a scoped window_id",
            );
        }

        let observation = if screenshot_metadata {
            match self.broker.observe(&self.session) {
                Ok(observation) => observation,
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            let windows = match self.broker.window_list(&self.session) {
                Ok(windows) => windows,
                Err(error) => return tool_broker_error(&error),
            };
            let active_window = windows
                .iter()
                .find(|window| window.active)
                .map(|window| window.id.clone());
            alice_computer_use_core::ComputerObservation {
                session_id: self.session.computer_session_id.clone(),
                screens: Vec::new(),
                windows,
                active_window,
                screenshot: None,
                frame: None,
                display_topology: None,
            }
        };
        let target_window = window_id.as_ref().and_then(|id| {
            observation
                .windows
                .iter()
                .find(|window| window.id == *id)
                .cloned()
        });
        if window_id.is_some() && target_window.is_none() {
            return tool_error("INVALID_WINDOW", "window_id is not current in this session");
        }

        let capability_profile = if capabilities {
            let Some(window_id) = window_id.as_ref() else {
                return tool_error(
                    "INVALID_ARGUMENTS",
                    "capabilities=true requires a scoped window_id",
                );
            };
            match self.broker.capability_probe(&self.session, window_id) {
                Ok(profile) => Some(profile),
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            None
        };

        let semantic_observation = if let Some(window_id) = window_id.as_ref().filter(|_| semantic)
        {
            match self
                .broker
                .semantic_observe(&self.session, window_id, limits)
            {
                Ok(observation) => Some(observation),
                Err(error) => return tool_broker_error(&error),
            }
        } else {
            None
        };
        let generation = semantic_observation
            .as_ref()
            .map(|value| value.metadata.generation);
        let truncated = semantic_observation
            .as_ref()
            .map(|value| value.metadata.truncated)
            .unwrap_or(false);
        let screenshot = observation.frame.as_ref().map(|value| {
            json!({
                "width": value.width,
                "height": value.height,
                "format": "bgra8",
                "coordinate_space": value.coordinate_space,
                "frame_id": value.frame_id,
                "display_id": value.display_id,
                "desktop_origin": value.desktop_origin,
                "topology_generation": value.topology_generation,
                "pixel_format": value.pixel_format,
                "stride": value.stride,
                "dpi": value.dpi,
                "scale": value.scale,
                "captured_at": value.captured_at,
                "stale_topology": value.stale_topology,
            })
        });
        let structured = json!({
            "session_id": self.session.computer_session_id,
            "windows": observation.windows,
            "target_window": target_window,
            "display": observation.screens,
            "display_topology": observation.display_topology,
            "semantic": semantic_observation,
            "capability_profile": capability_profile,
            "generation": generation,
            "truncated": truncated,
            "screenshot": screenshot,
            "capabilities": {
                "window_enumeration": true,
                "display_metadata": screenshot_metadata,
                "screenshot": screenshot_metadata,
                "semantic_observation": semantic,
                "execution_policy": true,
            },
        });
        tool_success(structured, "computer observation ready")
    }

    fn execute(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let intent = match args.object("intent") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let operation = match string_field(intent, "operation") {
            Ok(value) => value,
            Err(error) => return error,
        };
        let strategy = match args.strategy() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let fallback_policy = match args.fallback_policy() {
            Ok(value) => value,
            Err(error) => return error,
        };
        let intent = match operation.as_str() {
            "focus" | "invoke" | "set_value" | "toggle" | "select" | "expand" | "collapse"
            | "set_range_value" | "scroll_into_view" => {
                let element_id = match string_field(intent, "element_id") {
                    Ok(value) => ElementId::new(value),
                    Err(error) => return error,
                };
                let action = match operation.as_str() {
                    "focus" => SemanticAction::Focus { element_id },
                    "invoke" => SemanticAction::Invoke { element_id },
                    "set_value" => {
                        let value = match string_field(intent, "value") {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        SemanticAction::SetValue { element_id, value }
                    }
                    "toggle" => SemanticAction::Toggle { element_id },
                    "select" => SemanticAction::Select { element_id },
                    "expand" => SemanticAction::Expand { element_id },
                    "collapse" => SemanticAction::Collapse { element_id },
                    "set_range_value" => {
                        let value = match number_field(intent, "value") {
                            Ok(value) => value,
                            Err(error) => return error,
                        };
                        SemanticAction::SetRangeValue { element_id, value }
                    }
                    "scroll_into_view" => SemanticAction::ScrollIntoView { element_id },
                    _ => unreachable!(),
                };
                ComputerExecutionIntent::Semantic(action)
            }
            "mouse_down" | "mouse_up" | "middle_click" | "triple_click" | "modifier_click"
            | "key_down" | "key_up" | "hold_key" => {
                let target_window_id = match string_field(intent, "target_window_id") {
                    Ok(value) => WindowId::new(value),
                    Err(error) => return error,
                };
                let at = if matches!(
                    operation.as_str(),
                    "mouse_down" | "mouse_up" | "middle_click" | "triple_click" | "modifier_click"
                ) {
                    match coordinate_field(intent, "at") {
                        Ok(value) => Some(value),
                        Err(error) => return error,
                    }
                } else {
                    None
                };
                let action = match operation.as_str() {
                    "mouse_down" => ComputerAction::MouseDown {
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("mouse_down coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "mouse_up" => ComputerAction::MouseUp {
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("mouse_up coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "middle_click" => ComputerAction::MiddleClick {
                        at: at.expect("middle_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "triple_click" => ComputerAction::TripleClick {
                        at: at.expect("triple_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "modifier_click" => ComputerAction::ModifierClick {
                        modifier: match string_field(intent, "modifier") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        button: match mouse_button_field(intent, "button") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        at: at.expect("modifier_click coordinate was admitted"),
                        target: Some(target_window_id.clone()),
                    },
                    "key_down" => ComputerAction::KeyDown {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    "key_up" => ComputerAction::KeyUp {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    "hold_key" => ComputerAction::HoldKey {
                        key: match string_field(intent, "key") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        duration_ms: match number_u32_field(intent, "duration_ms") {
                            Ok(value) => value,
                            Err(error) => return error,
                        },
                        target: Some(target_window_id.clone()),
                    },
                    _ => unreachable!(),
                };
                ComputerExecutionIntent::Pixel {
                    action,
                    target_window_id: Some(target_window_id),
                }
            }
            _ => return tool_error("INVALID_ARGUMENTS", "unsupported intent.operation"),
        };
        let request = ComputerExecutionRequest {
            intent,
            strategy,
            fallback_policy,
        };
        let request_id = format!("mcp-{}", self.next_request.fetch_add(1, Ordering::Relaxed));
        let lease = match self.broker.acquire_desktop_lease(&self.session) {
            Ok(lease) => lease,
            Err(error) => return tool_broker_error(&error),
        };
        let result = self.broker.execute(BrokerExecutionRequest {
            request_id,
            session: self.session.clone(),
            lease: lease.clone(),
            execution: request,
        });
        let _ = self.broker.release_desktop_lease(&lease);
        match result {
            Ok(result) => execution_tool_result(result),
            Err(error) => tool_broker_error(&error),
        }
    }

    fn screenshot(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let screen_id = match args.optional_string("screen_id") {
            Ok(value) => value.map(alice_computer_use_core::ScreenId::new),
            Err(error) => return error,
        };
        match self.broker.capture_frame(&self.session, screen_id) {
            Ok(frame) => {
                match self
                    .broker
                    .encode_frame(&self.session, &frame.frame_id, FrameEncoding::Png)
                {
                    Ok(encoded) => {
                        let screenshot = encoded.screenshot;
                        let metadata = json!({
                            "width": screenshot.metadata.width,
                            "height": screenshot.metadata.height,
                            "format": screenshot.metadata.mime_type,
                            "coordinate_space": screenshot.metadata.coordinate_space,
                            "frame_id": screenshot.metadata.frame_id,
                            "display_id": screenshot.metadata.display_id,
                            "desktop_origin": screenshot.metadata.desktop_origin,
                            "dpi": screenshot.metadata.dpi,
                            "scale": screenshot.metadata.scale,
                            "captured_at": screenshot.metadata.captured_at,
                            "bytes": screenshot.bytes.len(),
                            "encode_cache_hit": encoded.cache_hit,
                            "encode_micros": encoded.encode_micros,
                            "topology_generation": frame.topology_generation,
                            "pixel_format": frame.pixel_format,
                            "stride": frame.stride,
                            "stale_topology": frame.stale_topology,
                        });
                        let content = json!([
                            { "type": "text", "text": metadata.to_string() },
                            {
                                "type": "image",
                                "data": BASE64.encode(&screenshot.bytes),
                                "mimeType": screenshot.metadata.mime_type,
                            }
                        ]);
                        json!({
                            "content": content,
                            "structuredContent": { "metadata": metadata },
                            "isError": false,
                        })
                    }
                    Err(error) => tool_broker_error(&error),
                }
            }
            Err(error) => tool_broker_error(&error),
        }
    }

    fn validate(&self, arguments: Value) -> Value {
        let args = match ObjectArgs::new(arguments) {
            Ok(args) => args,
            Err(error) => return error,
        };
        let element_id = match args.optional_string("element_id") {
            Ok(value) => value.map(ElementId::new),
            Err(error) => return error,
        };
        let window_id = match args.optional_string("window_id") {
            Ok(value) => value.map(WindowId::new),
            Err(error) => return error,
        };
        if element_id.is_none() && window_id.is_none() {
            return tool_error("INVALID_ARGUMENTS", "element_id or window_id is required");
        }
        if let Some(window_id) = window_id {
            match self
                .broker
                .window_list(&self.session)
                .map(|windows| windows.into_iter().any(|window| window.id == window_id))
            {
                Ok(true) => {}
                Ok(false) => return tool_error("INVALID_WINDOW", "window is not current"),
                Err(error) => return tool_broker_error(&error),
            }
        }
        if let Some(element_id) = element_id {
            match self.broker.validate_element(&self.session, &element_id) {
                Ok(()) => tool_success(
                    json!({ "valid": true, "element_id": element_id }),
                    "element is current",
                ),
                Err(error) => tool_broker_error(&error),
            }
        } else {
            tool_success(json!({ "valid": true }), "window is current")
        }
    }

    fn shutdown(&self) {
        let _ = self.broker.shutdown();
    }
}

impl Drop for McpServer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

struct ObjectArgs {
    value: Map<String, Value>,
}

impl ObjectArgs {
    fn new(value: Value) -> Result<Self, Value> {
        value
            .as_object()
            .cloned()
            .map(|value| Self { value })
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", "tool arguments must be an object"))
    }

    fn object(&self, name: &str) -> Result<&Map<String, Value>, Value> {
        self.value
            .get(name)
            .and_then(Value::as_object)
            .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be an object")))
    }

    fn optional_string(&self, name: &str) -> Result<Option<String>, Value> {
        match self.value.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => {
                value.as_str().map(str::to_owned).map(Some).ok_or_else(|| {
                    tool_error("INVALID_ARGUMENTS", format!("{name} must be a string"))
                })
            }
        }
    }

    fn boolean(&self, name: &str, default: bool) -> Result<bool, Value> {
        match self.value.get(name) {
            None => Ok(default),
            Some(value) => value
                .as_bool()
                .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be boolean"))),
        }
    }

    fn u32(&self, name: &str, default: u32) -> Result<u32, Value> {
        match self.value.get(name) {
            None => Ok(default),
            Some(value) => value
                .as_u64()
                .and_then(|value| u32::try_from(value).ok())
                .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("{name} must be uint32"))),
        }
    }

    fn limits(&self) -> Result<SemanticObservationLimits, Value> {
        Ok(SemanticObservationLimits {
            max_depth: self.u32("max_depth", 8)?,
            max_elements: self.u32("max_elements", 512)?,
        }
        .bounded())
    }

    fn strategy(&self) -> Result<ComputerExecutionStrategy, Value> {
        match self
            .value
            .get("strategy")
            .and_then(Value::as_str)
            .unwrap_or("prefer_semantic")
        {
            "semantic_only" => Ok(ComputerExecutionStrategy::SemanticOnly),
            "pixel_only" => Ok(ComputerExecutionStrategy::PixelOnly),
            "prefer_semantic" => Ok(ComputerExecutionStrategy::PreferSemantic),
            "prefer_pixel" => Ok(ComputerExecutionStrategy::PreferPixel),
            value => Err(tool_error(
                "INVALID_ARGUMENTS",
                format!("unsupported strategy: {value}"),
            )),
        }
    }

    fn fallback_policy(&self) -> Result<ComputerFallbackPolicy, Value> {
        match self
            .value
            .get("fallback_policy")
            .and_then(Value::as_str)
            .unwrap_or("deny")
        {
            "allow" => Ok(ComputerFallbackPolicy::Allow),
            "deny" => Ok(ComputerFallbackPolicy::Deny),
            "require_explicit" => Ok(ComputerFallbackPolicy::RequireExplicit),
            value => Err(tool_error(
                "INVALID_ARGUMENTS",
                format!("unsupported fallback_policy: {value}"),
            )),
        }
    }
}

fn string_field(object: &Map<String, Value>, name: &str) -> Result<String, Value> {
    object
        .get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} is required")))
}

fn number_field(object: &Map<String, Value>, name: &str) -> Result<f64, Value> {
    object
        .get(name)
        .and_then(Value::as_f64)
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} must be number")))
}

fn number_u32_field(object: &Map<String, Value>, name: &str) -> Result<u32, Value> {
    object
        .get(name)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} must be uint32")))
}

fn coordinate_field(object: &Map<String, Value>, name: &str) -> Result<Coordinate, Value> {
    object
        .get(name)
        .cloned()
        .ok_or_else(|| tool_error("INVALID_ARGUMENTS", format!("intent.{name} is required")))
        .and_then(|value| {
            serde_json::from_value(value).map_err(|error| {
                tool_error(
                    "INVALID_ARGUMENTS",
                    format!("intent.{name} is not a valid coordinate: {error}"),
                )
            })
        })
}

fn mouse_button_field(object: &Map<String, Value>, name: &str) -> Result<MouseButton, Value> {
    match object.get(name).and_then(Value::as_str).unwrap_or("left") {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        value => Err(tool_error(
            "INVALID_ARGUMENTS",
            format!("intent.{name} must be left, right, or middle; got {value}"),
        )),
    }
}

fn initialize_result() -> Value {
    json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
    })
}

fn tool_definitions() -> Vec<Value> {
    vec![
        tool_definition(
            "computer_health",
            "Read sidecar health and adapter session status.",
            json!({ "type": "object", "additionalProperties": false }),
        ),
        tool_definition(
            "computer_observe",
            "Enumerate windows and optionally observe one semantic window.",
            json!({
                "type": "object",
                "properties": {
                    "window_id": { "type": "string", "description": "Opaque session-scoped window id." },
                    "semantic": { "type": "boolean", "default": false },
                    "capabilities": { "type": "boolean", "default": false, "description": "Read-only application capability profile for the scoped window." },
                    "screenshot_metadata": { "type": "boolean", "default": false },
                    "max_depth": { "type": "integer", "minimum": 1, "maximum": 64 },
                    "max_elements": { "type": "integer", "minimum": 1, "maximum": 4096 }
                },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_execute",
            "Execute one explicit semantic or pixel/input action through the shared execution policy.",
            json!({
                "type": "object",
                "required": ["intent"],
                "properties": {
                    "intent": {
                        "type": "object",
                        "required": ["operation"],
                        "properties": {
                            "operation": { "type": "string", "enum": ["focus", "invoke", "set_value", "toggle", "select", "expand", "collapse", "set_range_value", "scroll_into_view", "mouse_down", "mouse_up", "middle_click", "triple_click", "modifier_click", "key_down", "key_up", "hold_key"] },
                            "element_id": { "type": "string", "description": "Opaque generation-scoped element id for semantic actions." },
                            "value": { "description": "String for set_value; number for set_range_value." },
                            "target_window_id": { "type": "string", "description": "Opaque session-scoped window id for pixel/input actions." },
                            "at": { "type": "object", "description": "Backend-neutral screen coordinate with space, point, extent, and dpi metadata." },
                            "button": { "type": "string", "enum": ["left", "right", "middle"], "default": "left" },
                            "modifier": { "type": "string", "description": "Modifier name such as ctrl or shift." },
                            "key": { "type": "string" },
                            "duration_ms": { "type": "integer", "minimum": 1, "maximum": 5000 }
                        },
                        "additionalProperties": false
                    },
                    "strategy": { "type": "string", "enum": ["semantic_only", "pixel_only", "prefer_semantic", "prefer_pixel"], "default": "prefer_semantic" },
                    "fallback_policy": { "type": "string", "enum": ["allow", "deny", "require_explicit"], "default": "deny" }
                },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_screenshot",
            "Capture a PNG screenshot through the sidecar and return MCP image content.",
            json!({
                "type": "object",
                "properties": { "screen_id": { "type": "string" } },
                "additionalProperties": false
            }),
        ),
        tool_definition(
            "computer_validate",
            "Validate a current opaque window or semantic element id without refreshing or rebinding it.",
            json!({
                "type": "object",
                "properties": {
                    "window_id": { "type": "string" },
                    "element_id": { "type": "string" }
                },
                "minProperties": 1,
                "additionalProperties": false
            }),
        ),
    ]
}

fn tool_definition(name: &str, description: &str, input_schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": input_schema })
}

fn tool_success(structured: Value, text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }],
        "structuredContent": structured,
        "isError": false,
    })
}

fn tool_error(code: impl Into<String>, message: impl Into<String>) -> Value {
    let code = code.into();
    let message = message.into();
    json!({
        "content": [{ "type": "text", "text": format!("{code}: {message}") }],
        "structuredContent": { "error": { "code": code, "message": message } },
        "isError": true,
    })
}

fn tool_broker_error(error: &BrokerError) -> Value {
    let code = error.code().to_owned();
    let message = error.to_string();
    json!({
        "content": [{ "type": "text", "text": message }],
        "structuredContent": {
            "error": {
                "code": code,
                "message": message,
                "outcome_unknown": error.outcome_unknown(),
                "retryable": error.retryable(),
            }
        },
        "isError": true,
    })
}

fn execution_tool_result(result: alice_computer_use_core::ComputerExecutionResult) -> Value {
    let is_error = matches!(
        result.final_outcome,
        ComputerExecutionOutcome::StaleElement
            | ComputerExecutionOutcome::UnknownElement
            | ComputerExecutionOutcome::ElementUnavailable
            | ComputerExecutionOutcome::Disabled
            | ComputerExecutionOutcome::ReadOnly
            | ComputerExecutionOutcome::InvalidValue
            | ComputerExecutionOutcome::WindowNotForeground
            | ComputerExecutionOutcome::FocusDenied
            | ComputerExecutionOutcome::VerificationFailed
            | ComputerExecutionOutcome::OutcomeUnknown
            | ComputerExecutionOutcome::InvalidRequest
            | ComputerExecutionOutcome::InvalidTarget
            | ComputerExecutionOutcome::BackendError
            | ComputerExecutionOutcome::IntegrityMismatch
            | ComputerExecutionOutcome::ElevationRequired
            | ComputerExecutionOutcome::UipiDenied
            | ComputerExecutionOutcome::ProtectedDesktop
            | ComputerExecutionOutcome::SecurityContextUnavailable
            | ComputerExecutionOutcome::TargetUnavailable
    );
    let structured = serde_json::to_value(&result).unwrap_or_else(|error| {
        json!({
            "final_outcome": "backend_error",
            "error": { "code": "SERIALIZATION_ERROR", "message": error.to_string() }
        })
    });
    json!({
        "content": [{ "type": "text", "text": result.explanation }],
        "structuredContent": structured,
        "isError": is_error,
    })
}

fn format_broker_error(error: &BrokerError) -> String {
    format!("{}: {}", error.code(), error)
}

fn json_rpc_error(code: i64, message: impl Into<String>) -> Value {
    json!({ "code": code, "message": message.into() })
}

fn sidecar_path() -> PathBuf {
    let mut args = env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--sidecar" {
            if let Some(path) = args.next() {
                return PathBuf::from(path);
            }
        }
    }
    if cfg!(windows) {
        PathBuf::from("target/release/alice-computer.exe")
    } else {
        PathBuf::from("target/release/alice-computer")
    }
}

fn run() -> AdapterResult<()> {
    let server = McpServer::new(sidecar_path())?;
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    let mut line = String::new();
    loop {
        line.clear();
        let count = input
            .read_line(&mut line)
            .map_err(|error| format!("MCP stdin read failed: {error}"))?;
        if count == 0 {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let request = match serde_json::from_str::<JsonRpcRequest>(&line) {
            Ok(request) => request,
            Err(error) => {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": json_rpc_error(-32700, format!("invalid JSON: {error}")),
                });
                serde_json::to_writer(&mut output, &response)
                    .map_err(|write_error| format!("MCP stdout write failed: {write_error}"))?;
                output
                    .write_all(b"\n")
                    .map_err(|write_error| format!("MCP stdout write failed: {write_error}"))?;
                output
                    .flush()
                    .map_err(|write_error| format!("MCP stdout flush failed: {write_error}"))?;
                continue;
            }
        };
        if let Some(response) = server.handle(request) {
            serde_json::to_writer(&mut output, &response)
                .map_err(|error| format!("MCP stdout write failed: {error}"))?;
            output
                .write_all(b"\n")
                .map_err(|error| format!("MCP stdout write failed: {error}"))?;
            output
                .flush()
                .map_err(|error| format!("MCP stdout flush failed: {error}"))?;
        }
    }
    server.shutdown();
    Ok(())
}

fn main() {
    if let Err(error) = run() {
        eprintln!("{SERVER_NAME}: {error}");
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn r9_keeps_exactly_five_mcp_tools_and_adds_profile_metadata_only_to_observe() {
        let tools = tool_definitions();
        assert_eq!(tools.len(), 5);
        assert!(!tools
            .iter()
            .any(|tool| tool["name"] == "computer_capabilities"));
        let observe = tools
            .iter()
            .find(|tool| tool["name"] == "computer_observe")
            .expect("computer_observe remains available");
        assert_eq!(
            observe["inputSchema"]["properties"]["capabilities"]["type"],
            "boolean"
        );
    }
}
